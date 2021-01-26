use bytes::Bytes;
use futures::{ready, stream::FusedStream, Stream};
use parking_lot::Mutex;
use pin_project_lite::pin_project;
use std::{borrow::Borrow, pin::Pin};
use zstd_seekable::{self, CStream, SeekableCStream};

pin_project! {
    pub struct Compress<S> {
        #[pin]
        stream: S,
        cstream: Mutex<SeekableCStream>,
        buf_out: Box<[u8]>,
        wrote_seek_table: bool,
    }
}

impl<S> std::fmt::Debug for Compress<S>
where
    S: Stream + std::fmt::Debug,
    S::Item: std::fmt::Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Compress")
            .field("stream", &self.stream)
            // .field("cstream", &self.cstream)
            .field("buf_out", &self.buf_out)
            .field("wrote_seek_table", &self.wrote_seek_table)
            .finish()
    }
}

pub trait StreamCompress {
    fn compress(self, compression_level: usize, frame_size: usize) -> ZstdError<Compress<Self>>
    where
        Self: Stream + Sized,
        Self::Item: std::borrow::Borrow<[u8]>;
}

impl<S> StreamCompress for S {
    fn compress(self, compression_level: usize, frame_size: usize) -> ZstdError<Compress<Self>>
    where
        // By having the bounds at the function level rather than trait level,
        // we get a better error message: when trying to use compress(), we'll
        // get a message pointing at the call stating which bounds are not met.
        // This is in contrast to putting the bounds on the impl itself: when we
        // do that, we get messages about the method not being found at all and
        // given a more cryptic error message about missing bounds on the S.
        S: Stream + Sized,
        S::Item: std::borrow::Borrow<[u8]>,
    {
        Compress::new(self, compression_level, frame_size)
    }
}

impl<S> Compress<S> {
    fn new(stream: S, compression_level: usize, frame_size: usize) -> ZstdError<Self>
    where
        S: Stream,
    {
        let cstream =
            parking_lot::const_mutex(SeekableCStream::new(compression_level, frame_size)?);
        let buf_out = vec![0; CStream::out_size()].into_boxed_slice();
        Ok(Self {
            stream,
            cstream,
            buf_out,
            wrote_seek_table: false,
        })
    }

    fn next_input(
        self: &mut Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<S::Item>>
    where
        S: Stream,
    {
        self.as_mut().project().stream.poll_next(cx)
    }

    fn compress_input(self: &mut Pin<&mut Self>, mut input: &[u8]) -> ZstdError<bytes::Bytes> {
        // Don't bother doing anything at all if we didn't get any input in.
        if input.is_empty() {
            return Ok(Bytes::new());
        }

        let this = self.as_mut().project();
        let cstream: &mut SeekableCStream = this.cstream.get_mut();
        let buf_out: &mut [u8] = this.buf_out;
        // It might seem wasteful to make a vector even if we end up only
        // decompressing once. However, Bytes::copy_from_slice just makes a
        // vector anyway and converts from there.
        let mut compressed_bytes = Vec::new();
        while input.is_empty() {
            let (out_pos, in_pos) = cstream.compress(buf_out, input)?;
            compressed_bytes.extend_from_slice(&buf_out[..out_pos]);
            input = &input[in_pos..];
        }
        Ok(bytes::Bytes::from(compressed_bytes))
    }

    fn end_stream(self: &mut Pin<&mut Self>) -> ZstdError<Bytes> {
        let this = self.as_mut().project();
        let wrote_seek_table = this.wrote_seek_table;
        let cstream: &mut Mutex<SeekableCStream> = this.cstream;
        let buf_out: &mut [u8] = this.buf_out;

        let mut cstream = cstream.lock();
        let mut out_pos = cstream.end_stream(buf_out)?;
        let mut compressed_bytes = (&buf_out[..out_pos]).to_vec();
        while out_pos > 0 {
            out_pos = cstream.end_stream(buf_out)?;
            compressed_bytes.extend_from_slice(&buf_out[..out_pos])
        }
        *wrote_seek_table = true;
        Ok(Bytes::from(compressed_bytes))
    }

    fn finished(self: &mut Pin<&mut Self>) -> bool {
        *self.as_mut().project().wrote_seek_table
    }
}

type ZstdError<A> = std::result::Result<A, zstd_seekable::Error>;

impl<S> Stream for Compress<S>
where
    S: Stream,
    S::Item: std::borrow::Borrow<[u8]>,
{
    type Item = ZstdError<Bytes>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        // We've already consumed everything and finalised our compression
        // stream. Yield nothing. Notably, we don't want to poke the upstream
        // again.
        if self.finished() {
            return std::task::Poll::Ready(None);
        }

        std::task::Poll::Ready(loop {
            match ready!(self.next_input(cx)) {
                None => match self.end_stream() {
                    Err(e) => break Some(Err(e)),
                    Ok(compressed_data) => {
                        if compressed_data.is_empty() {
                            break None;
                        } else {
                            break Some(Ok(compressed_data));
                        }
                    }
                },
                Some(bytes) => match self.compress_input(bytes.borrow()) {
                    Err(e) => break Some(Err(e)),
                    Ok(compressed_data) => {
                        // Maybe we want to return 0 length Bytes unconditionally?
                        // Who knows.
                        if !compressed_data.is_empty() {
                            break Some(Ok(compressed_data));
                        }
                    }
                },
            }
        })
    }
}

impl<S> FusedStream for Compress<S>
where
    S: Stream,
    S::Item: std::borrow::Borrow<[u8]>,
{
    fn is_terminated(&self) -> bool {
        self.wrote_seek_table
    }
}

pub struct SeekableCompress<'a, A: std::io::Write> {
    cstream: SeekableCStream,
    out: &'a mut A,
    // Tracks if we're done.
    wrote_seek_table: bool,
}

impl<'a, A: std::io::Write> SeekableCompress<'a, A> {
    pub fn new(
        out: &'a mut A,
        compression_level: usize,
        frame_size: usize,
    ) -> Result<Self, zstd_seekable::Error> {
        let cstream = SeekableCStream::new(compression_level, frame_size)?;

        Ok(SeekableCompress {
            cstream,
            out,
            wrote_seek_table: false,
        })
    }

    fn end_compression_internal(&mut self) -> std::io::Result<usize> {
        let mut buff_out = &mut *self.out_buffer();
        let mut wrote = 0;
        loop {
            let out_pos = self
                .cstream
                .end_stream(&mut buff_out)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
            // No more data left in the stream, we're done.
            if out_pos == 0 {
                break;
            }
            wrote += self.out.write(&buff_out[..out_pos])?
        }
        Ok(wrote)
    }

    // Finish compressing the object. This writes out any remaining data in the
    // compressor and writes the final seek table to the end of the data. You
    // should always call this before the object is dropped: Drop will invoke
    // this function but will ignore any errors which is probably not something
    // you want to do: likely you'll end up with file that can't be decompressed
    // at the end or if you get very lucky, that can be fully decompressed but
    // not in a seeking fashion.
    //
    // Returns number of compressed bytes we ended up writing as part of the end
    // of the stream.
    pub fn end_compression(mut self) -> std::io::Result<usize> {
        // We don't have to check if we ended the stream already here: this
        // method can be invoked precisely once. The only other entry point to
        // end_compression_internal is the drop implementation and that will
        // perform its own check.
        let r = self.end_compression_internal();
        self.wrote_seek_table = true;
        r
    }

    // We make the output buffer on demand. The reasonining is that we don't
    // want to balloon the memory usage if we have many SeekableCompress
    // objects. The compression time should greatly outweigh the time it takes
    // to allocate the output buffer. If not, we can move a shared buffer into
    // the structure and just re-use it.
    fn out_buffer(&self) -> Box<[u8]> {
        vec![0; CStream::out_size()].into_boxed_slice()
    }
}

impl<'a, A: std::io::Write> Drop for SeekableCompress<'a, A> {
    fn drop(&mut self) {
        // The user may have already called end_compression in which case we
        // don't want to re-invoke the internal function.
        if !self.wrote_seek_table {
            // We shouldn't panic in drop() so we suppress any error. User is
            // supposed to call end_compression themselves.
            let _e = self.end_compression_internal();
        }
    }
}

impl<'a, A> std::io::Write for SeekableCompress<'a, A>
where
    A: std::io::Write,
{
    fn write(&mut self, mut buf: &[u8]) -> std::io::Result<usize> {
        let starting_len = buf.len();
        let mut buff_out = &mut *self.out_buffer();

        // We loop until whole input is consumed. However, perhaps we should
        // not: the documentation only says that it's OK to only consume part of
        // the input. Perhaps we should leave it to the user to re-supply rest
        // of input? I'm unsure what the normal behaviour is supposed to be.
        while !buf.is_empty() {
            let (out_pos, in_pos) =
                SeekableCStream::compress(&mut self.cstream, &mut buff_out, buf)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
            let compressed_data = &buff_out[..out_pos];
            // Maybe we didn't get any data to write yet and it lives in the
            // stream. Don't do any write in that case.
            if !compressed_data.is_empty() {
                self.out.write_all(compressed_data)?;
            }
            buf = &buf[in_pos..];
        }
        // We consumed the whole input. Note that we report back the length of
        // the original buffer rather than sum of self.out.write: we don't want
        // to report back the number of compressed bytes but rather how much of
        // the input buffer was consumed.
        Ok(starting_len)
    }

    // Calling flush does nothing to our stream but we do invoke the underlying
    // object flush.
    fn flush(&mut self) -> std::io::Result<()> {
        self.out.flush()
    }
}

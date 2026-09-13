use std::{
    collections::VecDeque,
    io::{self, Read},
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant},
};

// Each codec has its own bounded ring; frames/pages are limited to 8192 bytes.
const BUFFER_FRAMES: usize = 256;
const BURST_FRAMES: u64 = 16;
pub const SOURCE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Default)]
struct State {
    headers: Vec<Arc<[u8]>>,
    frames: VecDeque<Arc<[u8]>>,
    next: u64,
    last_frame: Option<Instant>,
    closed: bool,
}

#[derive(Default)]
pub struct Broadcast {
    state: Mutex<State>,
    changed: Condvar,
}

impl Broadcast {
    pub fn set_headers(&self, headers: Vec<Vec<u8>>) {
        self.state.lock().unwrap().headers = headers.into_iter().map(Arc::from).collect();
    }

    pub fn headers(&self) -> Vec<Arc<[u8]>> {
        self.state.lock().unwrap().headers.clone()
    }

    pub fn publish(&self, frame: Vec<u8>) {
        let mut state = self.state.lock().unwrap();
        if state.frames.len() == BUFFER_FRAMES {
            state.frames.pop_front();
        }
        state.frames.push_back(frame.into());
        state.next += 1;
        state.last_frame = Some(Instant::now());
        self.changed.notify_all();
    }

    pub fn subscribe(&self) -> Option<u64> {
        let state = self.state.lock().unwrap();
        if state.closed || state.last_frame?.elapsed() >= SOURCE_TIMEOUT {
            None
        } else {
            Some(state.next.saturating_sub(BURST_FRAMES))
        }
    }

    // A listener whose cursor has fallen out of the ring is disconnected. It must
    // reconnect at the live edge instead of building an unbounded playback delay.
    pub fn next_frame(&self, cursor: &mut u64) -> Option<Arc<[u8]>> {
        let mut state = self.state.lock().unwrap();
        loop {
            let first = state.next - state.frames.len() as u64;
            if state.closed || *cursor < first {
                return None;
            }
            if *cursor < state.next {
                let frame = Arc::clone(&state.frames[(*cursor - first) as usize]);
                *cursor += 1;
                return Some(frame);
            }
            let (new_state, timeout) = self.changed.wait_timeout(state, SOURCE_TIMEOUT).unwrap();
            state = new_state;
            if timeout.timed_out() {
                return None;
            }
        }
    }

    pub fn stalled(&self, started: Instant) -> bool {
        self.state
            .lock()
            .unwrap()
            .last_frame
            .unwrap_or(started)
            .elapsed()
            >= SOURCE_TIMEOUT
    }

    pub fn close(&self) {
        self.state.lock().unwrap().closed = true;
        self.changed.notify_all();
    }
}

// Read complete ADTS frames from FFmpeg's pipe, never arbitrary pipe chunks:
// a late listener must begin with a decoder header, even when reads are fragmented.
pub fn read_frame(reader: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut header = [0; 7];
    reader.read_exact(&mut header)?;
    let length = ((header[3] as usize & 3) << 11)
        | ((header[4] as usize) << 3)
        | ((header[5] as usize) >> 5);
    let header_length = if header[1] & 1 == 0 { 9 } else { 7 };
    if header[0] != 0xff
        || header[1] & 0xf6 != 0xf0 // Sync word and layer 0; MPEG-2/4 both allowed.
        || header[2] >> 6 != 1 // AAC-LC.
        || (header[2] >> 2) & 15 > 12 // Reserved sample rate index.
        || length <= header_length
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid AAC-LC ADTS frame",
        ));
    }
    let mut frame = vec![0; length];
    frame[..7].copy_from_slice(&header);
    reader.read_exact(&mut frame[7..])?;
    Ok(frame)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_complete_frames_and_rejects_truncation() {
        let frame = [0xff, 0xf1, 0x4c, 0x80, 0x01, 0x3f, 0xfc, 42, 43];
        let mut input = [&frame[..], &frame[..]].concat();
        let mut reader = &input[..];
        assert_eq!(read_frame(&mut reader).unwrap(), frame);
        assert_eq!(read_frame(&mut reader).unwrap(), frame);
        assert!(reader.is_empty());
        input.pop();
        assert!(read_frame(&mut &input[9..]).is_err());
        input[0] = 0;
        assert_eq!(
            read_frame(&mut &input[..]).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn slow_listener_does_not_hold_back_live_listener() {
        let broadcast = Broadcast::default();
        assert!(broadcast.subscribe().is_none());
        broadcast.publish(vec![0]);
        let mut slow = broadcast.subscribe().unwrap();
        let mut fast = slow;
        assert_eq!(&*broadcast.next_frame(&mut fast).unwrap(), &[0]);
        for _ in 0..BUFFER_FRAMES + 1 {
            broadcast.publish(vec![1]);
            assert_eq!(&*broadcast.next_frame(&mut fast).unwrap(), &[1]);
        }
        assert!(broadcast.next_frame(&mut slow).is_none());
        assert_eq!(broadcast.state.lock().unwrap().frames.len(), BUFFER_FRAMES);
        let mut late = broadcast.subscribe().unwrap();
        assert_eq!(fast - late, BURST_FRAMES);
        assert!(broadcast.next_frame(&mut late).is_some());
        broadcast.close();
        assert!(broadcast.subscribe().is_none());
        assert!(broadcast.next_frame(&mut fast).is_none());
    }
}

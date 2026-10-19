use std::sync::{Mutex, Condvar};

/// 环形帧缓冲，存储BGRA像素帧
pub struct FrameRing {
    buf: Vec<Option<Vec<u8>>>,
    capacity: usize,
    write_pos: usize,
    read_pos: usize,
    count: usize,
    mutex: Mutex<()>,
    cv: Condvar,
}

impl FrameRing {
    pub fn new(capacity: usize) -> Self {
        Self {
            buf: vec![None; capacity],
            capacity,
            write_pos: 0,
            read_pos: 0,
            count: 0,
            mutex: Mutex::new(()),
            cv: Condvar::new(),
        }
    }

    pub fn push_frame(&mut self, frame: Vec<u8>) -> bool {
        let _lock = self.mutex.lock().unwrap();
        if self.count >= self.capacity {
            self.read_pos = (self.read_pos + 1) % self.capacity;
            self.count -= 1;
        }
        self.buf[self.write_pos] = Some(frame);
        self.write_pos = (self.write_pos + 1) % self.capacity;
        self.count += 1;
        self.cv.notify_one();
        true
    }

    pub fn pop_frame(&mut self) -> Option<Vec<u8>> {
        let mut lock = self.mutex.lock().unwrap();
        while self.count == 0 {
            lock = self.cv.wait(lock).unwrap();
        }
        let frame = self.buf[self.read_pos].take();
        self.read_pos = (self.read_pos + 1) % self.capacity;
        self.count -= 1;
        frame
    }

    pub fn clear(&mut self) {
        let _lock = self.mutex.lock().unwrap();
        for slot in &mut self.buf {
            *slot = None;
        }
        self.write_pos = 0;
        self.read_pos = 0;
        self.count = 0;
    }
}

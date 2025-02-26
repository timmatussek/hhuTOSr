use crate::lfb::LFB;
use alloc::vec::Vec;

pub struct BufferedLFB {
    buffer: Vec<u8>,
    lfb: LFB,
    target_lfb: LFB,
}

impl BufferedLFB {
    pub fn new(lfb: LFB) -> Self {
        let buffer = Vec::with_capacity((lfb.height() * lfb.pitch()) as usize);
        let raw_buffer = buffer.as_ptr() as *mut u8;

        Self { buffer, lfb: LFB::new(raw_buffer, lfb.pitch(), lfb.width(), lfb.height(), lfb.bpp()), target_lfb: lfb }
    }

    pub fn lfb(&mut self) -> &mut LFB {
        &mut self.lfb
    }

    pub fn direct_lfb(&mut self) -> &mut LFB {
        &mut self.target_lfb
    }

    pub fn flush(&mut self) {
        unsafe { self.target_lfb.buffer().copy_from(self.buffer.as_ptr(), (self.lfb.height() * self.lfb.pitch()) as usize); }
    }
}

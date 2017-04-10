
// some stuff for unsigned ints not present in Rust std. library
pub trait UIntExtras {
    // Find first set
    fn ffs(&self) -> Self;
    fn clear_tag(&self, mask: Self) -> Self;
}

impl UIntExtras for usize {
    fn clear_tag(&self, mask:Self) -> Self {
        self & !mask
    }
    
    fn ffs(&self) -> Self {
        let mut n = self ^ (self - 1);
        let mut bits = 0;
        while n > 0 {
            n >>= 1;
            bits += 1;
        }
        bits
    }
}

impl UIntExtras for u32 {
    fn clear_tag(&self, mask:Self) -> Self {
        self & !mask
    }
    
    // TODO: use bfs assembly instruction on x86
    fn ffs(&self) -> Self {
        let mut n = self ^ (self - 1);
        let mut bits = 0;
        while n > 0 {
            n >>= 1;
            bits += 1;
        }
        bits
    }
}

impl UIntExtras for u16 {
    fn clear_tag(&self, mask:Self) -> Self {
        self & !mask
    }
    
    // TODO: use bfs assembly instruction on x86
    fn ffs(&self) -> Self {
        let mut n = self ^ (self - 1);
        let mut bits = 0;
        while n > 0 {
            n >>= 1;
            bits += 1;
        }
        bits
    }
}

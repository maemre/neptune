use std::ops::Range;

pub trait BitField {
    fn bit_length(&self) -> u8;
    /// This function will panic for invalid bit indexes only during debugging.
    fn get_bit(&self, bit: u8) -> bool;
    /// This function will panic for invalid bit indexes only during debugging.
    fn set_bit(&mut self, bit: u8, value: bool) -> &mut Self;
    /// This function will panic for invalid bit ranges only during debugging.
    fn get_bits(&self, range: Range<u8>) -> Self;
    /// This function will panic for invalid bit ranges only during debugging.
    fn set_bits(&mut self, range: Range<u8>, value: Self) -> &mut Self;
}

macro_rules! bitfield_impl_for_int {
    ($($t:ty)*) => ($(
        impl BitField for $t {
            #[inline(always)]
            fn bit_length(&self) -> u8 {
                (::std::mem::size_of::<Self>() * 8) as u8
            }

            #[inline(always)]
            fn get_bit(&self, bit: u8) -> bool {
                debug_assert!(bit < self.bit_length());
                (self & (1 << bit)) != 0
            }

            #[inline(always)]
            fn set_bit(&mut self, bit: u8, value: bool) -> &mut Self {
                debug_assert!(bit < self.bit_length());
                if value {
                    *self |= 1 << bit;
                } else {
                    *self &= !(1 << bit);
                }

                self
            }

            #[inline(always)]
            fn get_bits(&self, range: Range<u8>) -> Self {
                debug_assert!(range.start < self.bit_length());
                debug_assert!(range.end <= self.bit_length());
                debug_assert!(range.start < range.end);
                // shift right to crush all set higher bits then do the same by shifting left
                *self << (self.bit_length() - range.end) >> (self.bit_length() - range.end) >> range.start
            }

            #[inline(always)]
            fn set_bits(&mut self, range: Range<u8>, value: Self) -> &mut Self {
                debug_assert!(range.start < self.bit_length());
                debug_assert!(range.end <= self.bit_length());
                debug_assert!(range.start < range.end);
                // value shifted by the beginning of offset should not overflow
                debug_assert!(value << (self.bit_length() - (range.end - range.start)) >>
                        (self.bit_length() - (range.end - range.start)) == value,
                        "value does not fit into bit range");
                // mask negating the range
                let bitmask: Self = !(!0 << (self.bit_length() - range.end) >>
                                      (self.bit_length() - range.end) >>
                                      range.start << range.start);

                // set bits
                *self = (*self & bitmask) | (value << range.start);

                self
            }
        }
    )*)
}

bitfield_impl_for_int! { u8 u16 u32 u64 usize i8 i16 i32 i64 isize }

// some stuff for unsigned ints not present in Rust std. library
pub trait UIntExtras {
    // Find first set
    fn ffs(&self) -> Self;
    fn clear_tag(&self, mask: Self) -> Self;
}

macro_rules! uintextras_impl {
    ($($t:ty)*) => ($(
        impl UIntExtras for $t {
            /// Clear tag with given mask.
            #[inline(always)]
            fn clear_tag(&self, mask:Self) -> Self {
                self & !mask
            }

            /// Find first set bit.
            // TODO: use bfs assembly instruction on x86 if this becomes a bottleneck
            #[inline(always)]
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

    )*)
}

uintextras_impl! { u8 u16 u32 u64 usize }

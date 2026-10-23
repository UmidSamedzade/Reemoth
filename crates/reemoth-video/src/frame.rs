use anyhow::Context;
use yuv::{BufferStoreMut, YuvConversionMode, YuvPlanarImageMut, YuvRange, YuvStandardMatrix};

/// Planar YUV 4:2:0, BT.709 limited range. Width and height are even.
#[derive(Clone)]
pub struct I420Frame {
    pub width: usize,
    pub height: usize,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
}

impl I420Frame {
    pub fn new(width: usize, height: usize) -> Self {
        let (cw, ch) = (width / 2, height / 2);
        Self {
            width,
            height,
            y: vec![16; width * height],
            u: vec![128; cw * ch],
            v: vec![128; cw * ch],
        }
    }

    /// Converts a BGRA image (`stride` bytes per row) into this frame,
    /// cropping odd edges. Reallocates if the size changed.
    pub fn fill_from_bgra(
        &mut self,
        bgra: &[u8],
        width: usize,
        height: usize,
        stride: usize,
    ) -> anyhow::Result<()> {
        let (w, h) = (width & !1, height & !1);
        if (w, h) != (self.width, self.height) {
            *self = Self::new(w, h);
        }
        let mut planar = YuvPlanarImageMut {
            y_plane: BufferStoreMut::Borrowed(&mut self.y),
            y_stride: w as u32,
            u_plane: BufferStoreMut::Borrowed(&mut self.u),
            u_stride: (w / 2) as u32,
            v_plane: BufferStoreMut::Borrowed(&mut self.v),
            v_stride: (w / 2) as u32,
            width: w as u32,
            height: h as u32,
        };
        yuv::bgra_to_yuv420(
            &mut planar,
            bgra,
            stride as u32,
            YuvRange::Limited,
            YuvStandardMatrix::Bt709,
            YuvConversionMode::Balanced,
        )
        .context("BGRA to I420 conversion")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_solid_colors_to_bt709_limited() {
        let (w, h) = (6, 4);
        let mut bgra = vec![0u8; w * h * 4];
        // White everywhere.
        bgra.iter_mut().for_each(|b| *b = 255);
        let mut f = I420Frame::new(2, 2);
        f.fill_from_bgra(&bgra, w, h, w * 4).unwrap();
        assert_eq!((f.width, f.height), (6, 4));
        assert!(f.y.iter().all(|&y| (234..=236).contains(&y)), "{:?}", f.y);
        assert!(f.u.iter().all(|&u| (127..=129).contains(&u)));

        // Pure red in BGRA is (0, 0, 255, 255): BT.709 Y ≈ 63, V high.
        for px in bgra.chunks_mut(4) {
            px.copy_from_slice(&[0, 0, 255, 255]);
        }
        f.fill_from_bgra(&bgra, w, h, w * 4).unwrap();
        assert!(f.y.iter().all(|&y| (60..=66).contains(&y)), "{:?}", f.y);
        assert!(f.v.iter().all(|&v| v > 220), "{:?}", f.v);
    }

    #[test]
    fn crops_odd_dimensions() {
        let bgra = vec![0u8; 5 * 3 * 4];
        let mut f = I420Frame::new(2, 2);
        f.fill_from_bgra(&bgra, 5, 3, 20).unwrap();
        assert_eq!((f.width, f.height, f.u.len()), (4, 2, 2));
    }
}

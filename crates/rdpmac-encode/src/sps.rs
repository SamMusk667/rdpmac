//! Rewrites the sequence parameter set VideoToolbox writes, so that its VUI says what the stream
//! is: full-range BT.709, no frame reordering, and a decoded picture buffer as small as the
//! reference frames need.
//!
//! Without a bitstream restriction a decoder has to size its picture buffer for the level's
//! maximum. At 1920x1200, level 5.0, that is 12 frames, and mstsc on Windows dropped the
//! connection right after connecting (0x1108); at 1920x1080, level 4.0, it is 4 frames and works.
//! The explicit restriction also lets decoders show each frame as soon as it is decoded.

/// Profiles whose SPS carries chroma format and bit depth fields.
const HIGH_PROFILES: [u32; 13] = [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135];
const BT709: u32 = 1;
const UNSPECIFIED_FORMAT: u32 = 5;

struct Reader<'a> {
    data: &'a [u8],
    bit: usize,
}

impl Reader<'_> {
    fn u(&mut self, count: usize) -> Option<u32> {
        let mut value = 0u32;
        for _ in 0..count {
            let byte = *self.data.get(self.bit / 8)?;
            value = (value << 1) | u32::from((byte >> (7 - self.bit % 8)) & 1);
            self.bit += 1;
        }
        Some(value)
    }

    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.u(1)? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some((1u32 << zeros) - 1 + self.u(zeros)?)
    }

    fn se(&mut self) -> Option<()> {
        self.ue().map(|_| ())
    }
}

#[derive(Default)]
struct Writer {
    bytes: Vec<u8>,
    bits: usize,
}

impl Writer {
    fn u(&mut self, count: usize, value: u32) {
        for i in (0..count).rev() {
            if self.bits.is_multiple_of(8) {
                self.bytes.push(0);
            }
            if (value >> i) & 1 == 1 {
                let last = self.bytes.len() - 1;
                self.bytes[last] |= 0x80 >> (self.bits % 8);
            }
            self.bits += 1;
        }
    }

    fn ue(&mut self, value: u32) {
        let coded = u64::from(value) + 1;
        let length = 64 - coded.leading_zeros() as usize;
        self.u(length - 1, 0);
        for i in (0..length).rev() {
            self.u(1, ((coded >> i) & 1) as u32);
        }
    }

    fn trailing_bits(&mut self) {
        self.u(1, 1);
        while !self.bits.is_multiple_of(8) {
            self.u(1, 0);
        }
    }
}

/// Removes emulation prevention bytes: 00 00 03 becomes 00 00.
fn unescape(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len());
    let mut zeros = 0;
    for &byte in payload {
        if zeros >= 2 && byte == 3 {
            zeros = 0;
            continue;
        }
        zeros = if byte == 0 { zeros + 1 } else { 0 };
        out.push(byte);
    }
    out
}

/// Inserts emulation prevention bytes so no start code appears inside the unit.
fn escape(rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rbsp.len() + 8);
    let mut zeros = 0;
    for &byte in rbsp {
        if zeros >= 2 && byte <= 3 {
            out.push(3);
            zeros = 0;
        }
        zeros = if byte == 0 { zeros + 1 } else { 0 };
        out.push(byte);
    }
    out
}

/// Returns the SPS NAL unit (header byte first) with our VUI in place of whatever it had, or
/// `None` when the unit is not an SPS this can parse; the caller then keeps the original.
pub fn with_vui(nal: &[u8]) -> Option<Vec<u8>> {
    let (&header, payload) = nal.split_first()?;
    if header & 0x1f != 7 {
        return None;
    }
    let rbsp = unescape(payload);
    let mut r = Reader { data: &rbsp, bit: 0 };
    let profile = r.u(8)?;
    r.u(16)?; // constraint flags, level
    r.ue()?; // seq_parameter_set_id
    if HIGH_PROFILES.contains(&profile) {
        if r.ue()? == 3 {
            r.u(1)?; // separate_colour_plane_flag
        }
        r.ue()?; // bit_depth_luma_minus8
        r.ue()?; // bit_depth_chroma_minus8
        r.u(1)?; // qpprime_y_zero_transform_bypass_flag
        if r.u(1)? == 1 {
            return None; // scaling matrices: not worth parsing for VideoToolbox's output
        }
    }
    r.ue()?; // log2_max_frame_num_minus4
    match r.ue()? {
        0 => {
            r.ue()?; // log2_max_pic_order_cnt_lsb_minus4
        }
        1 => {
            r.u(1)?;
            r.se()?;
            r.se()?;
            for _ in 0..r.ue()? {
                r.se()?;
            }
        }
        _ => {}
    }
    let reference_frames = r.ue()?;
    r.u(1)?; // gaps_in_frame_num_value_allowed_flag
    r.ue()?; // pic_width_in_mbs_minus1
    r.ue()?; // pic_height_in_map_units_minus1
    if r.u(1)? == 0 {
        r.u(1)?; // mb_adaptive_frame_field_flag
    }
    r.u(1)?; // direct_8x8_inference_flag
    if r.u(1)? == 1 {
        for _ in 0..4 {
            r.ue()?; // frame cropping offsets
        }
    }
    let vui_flag_at = r.bit;
    r.u(1)?; // the VUI flag must exist even if we replace what follows it

    let mut w = Writer::default();
    let mut copy = Reader { data: &rbsp, bit: 0 };
    for _ in 0..vui_flag_at {
        w.u(1, copy.u(1)?);
    }
    w.u(1, 1); // vui_parameters_present_flag
    w.u(1, 0); // aspect_ratio_info_present_flag
    w.u(1, 0); // overscan_info_present_flag
    w.u(1, 1); // video_signal_type_present_flag
    w.u(3, UNSPECIFIED_FORMAT);
    w.u(1, 1); // video_full_range_flag
    w.u(1, 1); // colour_description_present_flag
    w.u(8, BT709); // colour_primaries
    w.u(8, BT709); // transfer_characteristics
    w.u(8, BT709); // matrix_coefficients
    w.u(1, 0); // chroma_loc_info_present_flag
    w.u(1, 0); // timing_info_present_flag
    w.u(1, 0); // nal_hrd_parameters_present_flag
    w.u(1, 0); // vcl_hrd_parameters_present_flag
    w.u(1, 0); // pic_struct_present_flag
    w.u(1, 1); // bitstream_restriction_flag
    w.u(1, 1); // motion_vectors_over_pic_boundaries_flag
    w.ue(2); // max_bytes_per_pic_denom, the default
    w.ue(1); // max_bits_per_mb_denom, the default
    w.ue(15); // log2_max_mv_length_horizontal, the default
    w.ue(15); // log2_max_mv_length_vertical
    w.ue(0); // max_num_reorder_frames
    w.ue(reference_frames.max(1)); // max_dec_frame_buffering
    w.trailing_bits();

    let mut out = vec![header];
    out.extend(escape(&w.bytes));
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The SPS VideoToolbox wrote for 1920x1200 from full-range input: Main, level 5.0, one
    /// reference frame, VUI with only the video signal type.
    const VT_1920X1200: [u8; 15] = [0x27, 0x4d, 0x00, 0x32, 0xab, 0x40, 0x3c, 0x01, 0x2f, 0x4d, 0xc0, 0x80, 0x80, 0x80, 0x80];

    struct Vui {
        full_range: u32,
        matrix: u32,
        reorder: u32,
        buffering: u32,
        width_mbs: u32,
        height_mbs: u32,
    }

    fn read_back(nal: &[u8]) -> Vui {
        let rbsp = unescape(&nal[1..]);
        let mut r = Reader { data: &rbsp, bit: 0 };
        assert_eq!(r.u(8), Some(77));
        r.u(16).unwrap();
        r.ue().unwrap();
        r.ue().unwrap();
        assert_eq!(r.ue(), Some(0));
        r.ue().unwrap();
        r.ue().unwrap();
        r.u(1).unwrap();
        let width_mbs = r.ue().unwrap() + 1;
        let height_mbs = r.ue().unwrap() + 1;
        assert_eq!(r.u(1), Some(1));
        r.u(1).unwrap();
        if r.u(1).unwrap() == 1 {
            for _ in 0..4 {
                r.ue().unwrap();
            }
        }
        assert_eq!(r.u(1), Some(1), "VUI present");
        assert_eq!(r.u(2), Some(0));
        assert_eq!(r.u(1), Some(1));
        r.u(3).unwrap();
        let full_range = r.u(1).unwrap();
        assert_eq!(r.u(1), Some(1));
        r.u(16).unwrap();
        let matrix = r.u(8).unwrap();
        assert_eq!(r.u(5), Some(0));
        assert_eq!(r.u(1), Some(1), "bitstream restriction");
        r.u(1).unwrap();
        for _ in 0..4 {
            r.ue().unwrap();
        }
        let reorder = r.ue().unwrap();
        let buffering = r.ue().unwrap();
        assert_eq!(r.u(1), Some(1), "stop bit");
        Vui {
            full_range,
            matrix,
            reorder,
            buffering,
            width_mbs,
            height_mbs,
        }
    }

    #[test]
    fn videotoolbox_sps_gains_restriction_and_bt709() {
        let rewritten = with_vui(&VT_1920X1200).expect("parsable");
        let vui = read_back(&rewritten);
        assert_eq!((vui.width_mbs, vui.height_mbs), (120, 75), "size untouched");
        assert_eq!((vui.full_range, vui.matrix), (1, BT709));
        assert_eq!((vui.reorder, vui.buffering), (0, 1));
    }

    #[test]
    fn exp_golomb_round_trips() {
        let mut w = Writer::default();
        for v in [0, 1, 2, 7, 8, 255, 1000] {
            w.ue(v);
        }
        w.trailing_bits();
        let mut r = Reader { data: &w.bytes, bit: 0 };
        for v in [0, 1, 2, 7, 8, 255, 1000] {
            assert_eq!(r.ue(), Some(v));
        }
    }

    #[test]
    fn emulation_prevention_round_trips() {
        let rbsp = [0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x05, 0x00, 0x00, 0x03];
        let escaped = escape(&rbsp);
        assert!(!escaped.windows(3).any(|w| w[0] == 0 && w[1] == 0 && w[2] <= 2));
        assert_eq!(unescape(&escaped), rbsp);
    }

    #[test]
    fn other_units_are_left_alone() {
        assert!(with_vui(&[0x28, 0xee, 0x3c, 0x80]).is_none(), "PPS");
        assert!(with_vui(&[]).is_none());
    }
}

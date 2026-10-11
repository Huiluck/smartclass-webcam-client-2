//! H.264 and H.265 parameter-set bitstreams for the VAAPI packed headers.
//!
//! VAAPI hardware encoders do not emit sequence or picture parameter sets: the application
//! hands them over as "packed headers" and the driver splices them — and, for the slice
//! headers it generates itself, the syntax *derived from them* — into the stream. The
//! settings here are therefore the single source of truth: the encoder fills its
//! `VAEnc*ParameterBuffer*` structs from the same fields this module writes into the packed
//! RBSPs, so the two cannot drift apart.
//!
//! The values mirror what FFmpeg's VAAPI encoders use, which is what the drivers are tested
//! against: a low-delay IPPP stream with one reference frame, `pic_order_cnt_type == 2` for
//! H.264 (no reordering, no B frames), a 2-second GOP, and no HRD, SEI or scaling lists.
//!
//! Everything is emitted as an Annex B NAL unit, with the 4-byte start code and with
//! emulation-prevention bytes already inserted, because that is the form
//! `VAEncPackedHeaderParameterBuffer::has_emulation_bytes = 1` announces.

/// The H.264 profile the encoder was configured with.
///
/// The VAAPI config, the SPS `profile_idc` and the PPS's optional fields all have to agree,
/// so the choice travels as one value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum H264Profile {
    /// `VAProfileH264High`: CABAC and 8x8 transforms, the profile every current decoder
    /// handles.
    High,
    /// `VAProfileH264Main`: CABAC without 8x8 transforms.
    Main,
    /// `VAProfileH264ConstrainedBaseline`: no CABAC; the fallback for old drivers.
    ConstrainedBaseline,
}

impl H264Profile {
    /// The `profile_idc` of the SPS.
    pub(crate) const fn profile_idc(self) -> u8 {
        match self {
            Self::High => 100,
            Self::Main => 77,
            Self::ConstrainedBaseline => 66,
        }
    }

    /// `constraint_set0..5_flag` plus the two reserved bits, as one byte.
    ///
    /// FFmpeg's values for the same profiles: Main and Constrained Baseline set
    /// `constraint_set1_flag` (the baseline-compatible flag), High sets
    /// `constraint_set4_flag` and — with no B frames — `constraint_set5_flag`.
    pub(crate) const fn constraint_byte(self) -> u8 {
        match self {
            // constraint_set1_flag
            Self::Main => 1 << 6,
            // constraint_set0_flag | constraint_set1_flag
            Self::ConstrainedBaseline => (1 << 7) | (1 << 6),
            // constraint_set4_flag | constraint_set5_flag
            Self::High => (1 << 3) | (1 << 2),
        }
    }

    /// Whether the profile carries the High-profile SPS fields (`chroma_format_idc`, bit
    /// depths, scaling matrices) and the PPS's trailing fields.
    pub(crate) const fn is_high(self) -> bool {
        matches!(self, Self::High)
    }
}

/// Sequence-level settings of an H.264 stream, shared by the packed SPS/PPS and the
/// `VAEnc*ParameterBufferH264` structs.
#[derive(Debug, Clone, Copy)]
pub(crate) struct H264Settings {
    /// Coded picture width in pixels.
    pub(crate) width: u32,
    /// Coded picture height in pixels.
    pub(crate) height: u32,
    /// Frame rate, used for the VUI timing info.
    pub(crate) fps: u32,
    /// The profile both the SPS and the VAAPI config declare.
    pub(crate) profile: H264Profile,
    /// `level_idc` from [`H264Settings::level_idc`].
    pub(crate) level_idc: u8,
    /// Width in 16x16 macroblocks: `align(width, 16) / 16`.
    pub(crate) mb_width: u16,
    /// Height in 16x16 macroblocks: `align(height, 16) / 16`.
    pub(crate) mb_height: u16,
    /// Reference frames the decoder must keep, i.e. the DPB size.
    pub(crate) max_num_ref_frames: u32,
    /// `log2_max_frame_num_minus4`: 8-bit `frame_num` values, as FFmpeg uses.
    pub(crate) log2_max_frame_num_minus4: u8,
    /// `pic_order_cnt_type`. Always 0 here, so a picture order count is coded in every
    /// slice header: the driver writes slice headers of its own, and a count that is
    /// signalled is one it cannot get wrong.
    pub(crate) pic_order_cnt_type: u8,
    /// `log2_max_pic_order_cnt_lsb_minus4`: 8-bit picture order counts.
    pub(crate) log2_max_pic_order_cnt_lsb_minus4: u8,
    /// Whether the PPS enables CABAC.
    pub(crate) entropy_coding: bool,
    /// Whether the PPS enables 8x8 transforms; only meaningful for a High profile.
    pub(crate) transform_8x8: bool,
    /// QP the slices start at, minus 26 (the PPS's `pic_init_qp_minus26`).
    pub(crate) pic_init_qp_minus26: i32,
}

impl H264Settings {
    /// Derives the settings for one pipeline configuration.
    ///
    /// The surface geometry is macroblock-aligned and the SPS crops back to the requested
    /// size, which is what the driver expects of an encoder configuration: it codes whole
    /// macroblocks and lets the decoder discard the padding.
    pub(crate) fn new(width: u32, height: u32, fps: u32, profile: H264Profile) -> Self {
        let mb_width = width.div_ceil(16).max(1);
        let mb_height = height.div_ceil(16).max(1);
        Self {
            width,
            height,
            fps: fps.max(1),
            profile,
            level_idc: h264_level_idc(u64::from(mb_width) * u64::from(mb_height), fps),
            mb_width: mb_width.min(u16::MAX as u32) as u16,
            mb_height: mb_height.min(u16::MAX as u32) as u16,
            max_num_ref_frames: 1,
            log2_max_frame_num_minus4: 4,
            pic_order_cnt_type: 0,
            log2_max_pic_order_cnt_lsb_minus4: 4,
            entropy_coding: !matches!(profile, H264Profile::ConstrainedBaseline),
            transform_8x8: profile.is_high(),
            pic_init_qp_minus26: 0,
        }
    }

    /// The macroblock-aligned surface size the encoder codes.
    pub(crate) const fn surface_width(&self) -> u32 {
        (self.mb_width as u32) * 16
    }

    /// The macroblock-aligned surface height.
    pub(crate) const fn surface_height(&self) -> u32 {
        (self.mb_height as u32) * 16
    }

    /// `frame_crop_right_offset`, in the 2-sample units a 4:2:0 progressive frame uses.
    ///
    /// Shared with the encoder, whose sequence parameters carry the same values.
    pub(crate) fn crop_right(&self) -> u32 {
        (self.surface_width().saturating_sub(self.width)) / 2
    }

    /// `frame_crop_bottom_offset`.
    pub(crate) fn crop_bottom(&self) -> u32 {
        (self.surface_height().saturating_sub(self.height)) / 2
    }

    /// Writes `seq_parameter_set_rbsp()` as an Annex B NAL unit.
    pub(crate) fn write_sps(&self) -> Vec<u8> {
        let mut rbsp = BitWriter::new();
        rbsp.put_bits(u32::from(self.profile.profile_idc()), 8);
        rbsp.put_bits(u32::from(self.profile.constraint_byte()), 8);
        rbsp.put_bits(u32::from(self.level_idc), 8);
        rbsp.put_ue(0); // seq_parameter_set_id

        if self.profile.is_high() {
            rbsp.put_ue(1); // chroma_format_idc: 4:2:0
            rbsp.put_ue(0); // bit_depth_luma_minus8
            rbsp.put_ue(0); // bit_depth_chroma_minus8
            rbsp.put_bit(0); // qpprime_y_zero_transform_bypass_flag
            rbsp.put_bit(0); // seq_scaling_matrix_present_flag
        }

        rbsp.put_ue(u32::from(self.log2_max_frame_num_minus4));
        rbsp.put_ue(u32::from(self.pic_order_cnt_type));
        if self.pic_order_cnt_type == 0 {
            rbsp.put_ue(u32::from(self.log2_max_pic_order_cnt_lsb_minus4));
        }
        rbsp.put_ue(self.max_num_ref_frames);
        rbsp.put_bit(0); // gaps_in_frame_num_value_allowed_flag
        rbsp.put_ue(u32::from(self.mb_width) - 1); // pic_width_in_mbs_minus1
        rbsp.put_ue(u32::from(self.mb_height) - 1); // pic_height_in_map_units_minus1
        rbsp.put_bit(1); // frame_mbs_only_flag: progressive only
        rbsp.put_bit(1); // direct_8x8_inference_flag

        let cropping = self.crop_right() != 0 || self.crop_bottom() != 0;
        rbsp.put_bit(u32::from(cropping));
        if cropping {
            rbsp.put_ue(0); // frame_crop_left_offset
            rbsp.put_ue(self.crop_right());
            rbsp.put_ue(0); // frame_crop_top_offset
            rbsp.put_ue(self.crop_bottom());
        }

        rbsp.put_bit(1); // vui_parameters_present_flag
        self.write_vui(&mut rbsp);
        rbsp.rbsp_trailing_bits();

        annex_b_nal(0x67, &rbsp.finish())
    }

    /// Writes the VUI: timing info plus a small bitstream-restriction block.
    ///
    /// Everything optional is left out (aspect ratio, video signal type, chroma location,
    /// HRD), which keeps the hand-written syntax short: only the frame-rate timing a
    /// decoder needs to present the stream at the right speed is declared.
    fn write_vui(&self, rbsp: &mut BitWriter) {
        rbsp.put_bit(0); // aspect_ratio_info_present_flag
        rbsp.put_bit(0); // overscan_info_present_flag
        rbsp.put_bit(0); // video_signal_type_present_flag
        rbsp.put_bit(0); // chroma_loc_info_present_flag
        rbsp.put_bit(1); // timing_info_present_flag
        rbsp.put_bits(1, 32); // num_units_in_tick
        rbsp.put_bits(self.fps.saturating_mul(2), 32); // time_scale
        rbsp.put_bit(1); // fixed_frame_rate_flag
        rbsp.put_bit(0); // nal_hrd_parameters_present_flag
        rbsp.put_bit(0); // vcl_hrd_parameters_present_flag
        rbsp.put_bit(0); // pic_struct_present_flag
        rbsp.put_bit(1); // bitstream_restriction_flag
        rbsp.put_bit(1); // motion_vectors_over_pic_boundaries_flag
        rbsp.put_ue(0); // max_bytes_per_pic_denom
        rbsp.put_ue(0); // max_bits_per_mb_denom
        rbsp.put_ue(15); // log2_max_mv_length_horizontal
        rbsp.put_ue(15); // log2_max_mv_length_vertical
        rbsp.put_ue(0); // max_num_reorder_frames
        rbsp.put_ue(1); // max_dec_frame_buffering
    }

    /// Writes `pic_parameter_set_rbsp()` as an Annex B NAL unit.
    pub(crate) fn write_pps(&self) -> Vec<u8> {
        let mut rbsp = BitWriter::new();
        rbsp.put_ue(0); // pic_parameter_set_id
        rbsp.put_ue(0); // seq_parameter_set_id
        rbsp.put_bit(u32::from(self.entropy_coding));
        rbsp.put_bit(0); // bottom_field_pic_order_in_frame_present_flag
        rbsp.put_ue(0); // num_slice_groups_minus1
        rbsp.put_ue(0); // num_ref_idx_l0_default_active_minus1
        rbsp.put_ue(0); // num_ref_idx_l1_default_active_minus1
        rbsp.put_bit(0); // weighted_pred_flag
        rbsp.put_bits(0, 2); // weighted_bipred_idc
        rbsp.put_se(self.pic_init_qp_minus26);
        rbsp.put_se(0); // pic_init_qs_minus26
        rbsp.put_se(0); // chroma_qp_index_offset
        rbsp.put_bit(0); // deblocking_filter_control_present_flag
        rbsp.put_bit(0); // constrained_intra_pred_flag
        rbsp.put_bit(0); // redundant_pic_cnt_present_flag

        if self.profile.is_high() {
            // The High profile keeps reading past `redundant_pic_cnt_present_flag`.
            rbsp.put_bit(u32::from(self.transform_8x8));
            rbsp.put_bit(0); // pic_scaling_matrix_present_flag
            rbsp.put_se(0); // second_chroma_qp_index_offset
        }
        rbsp.rbsp_trailing_bits();

        annex_b_nal(0x68, &rbsp.finish())
    }
}

/// Chooses an H.264 level from the coded macroblock count and the frame rate.
///
/// A small table lookup over the level limits (maximum frame size in macroblocks, maximum
/// macroblocks per second, maximum bit rate) is enough here: the encoder needs a plausible
/// `level_idc`, and picking one that is too low is what breaks strict decoders. The bit
/// rate column is the High-profile budget, so the result is conservative for Main.
fn h264_level_idc(macroblocks: u64, fps: u32) -> u8 {
    /// `(level_idc, max frame size in MBs, max MBs per second, max bit rate in kbps)`.
    const LEVELS: [(u8, u64, u64, u64); 9] = [
        (30, 1620, 40_500, 10_000),
        (31, 3600, 108_000, 14_000),
        (32, 5120, 216_000, 20_000),
        (40, 8192, 245_760, 20_000),
        (41, 8192, 245_760, 50_000),
        (42, 8704, 522_240, 50_000),
        (50, 22_080, 589_824, 135_000),
        (51, 36_864, 983_040, 240_000),
        (52, 36_864, 2_073_600, 240_000),
    ];
    let per_second = macroblocks.saturating_mul(u64::from(fps.max(1)));
    for (level, max_frame_size, max_macroblock_rate, _) in LEVELS {
        if macroblocks <= max_frame_size && per_second <= max_macroblock_rate {
            return level;
        }
    }
    // Nothing fits: declare the highest level and let the server's bitrate cap do the
    // restricting. 52 is the top of the table above.
    52
}

/// The HEVC NAL unit types this backend emits.
pub(crate) mod hevc_nal {
    /// `VPS_NUT`.
    pub(crate) const VPS: u8 = 32;
    /// `SPS_NUT`.
    pub(crate) const SPS: u8 = 33;
    /// `PPS_NUT`.
    pub(crate) const PPS: u8 = 34;
    /// `TRAIL_R`: a non-IDR picture that may be referenced.
    pub(crate) const TRAIL_R: u8 = 1;
    /// `IDR_W_RADL`: an instantaneous decoder refresh picture.
    pub(crate) const IDR_W_RADL: u8 = 19;
}

/// Sequence-level settings of an H.265 stream, shared by the packed parameter sets and the
/// `VAEnc*ParameterBufferHEVC` structs.
#[derive(Debug, Clone, Copy)]
pub(crate) struct HevcSettings {
    /// Coded picture width in pixels.
    pub(crate) width: u32,
    /// Coded picture height in pixels.
    pub(crate) height: u32,
    /// Frame rate, for the VUI timing info.
    pub(crate) fps: u32,
    /// `general_profile_idc`; 1 is Main, the only profile this backend asks for.
    pub(crate) general_profile_idc: u8,
    /// `general_level_idc`, from [`HevcSettings::new`].
    pub(crate) general_level_idc: u8,
    /// `general_tier_flag`: always 0 (Main tier) for the levels and rates involved.
    pub(crate) general_tier_flag: u8,
    /// Width in luma samples of the coded surface (16-aligned).
    pub(crate) surface_width: u32,
    /// Height in luma samples of the coded surface (16-aligned).
    pub(crate) surface_height: u32,
    /// `log2_min_luma_coding_block_size_minus3`: 8x8 minimum coding units.
    pub(crate) log2_min_luma_coding_block_size_minus3: u8,
    /// `log2_diff_max_min_luma_coding_block_size`: 32x32 coding tree units.
    pub(crate) log2_diff_max_min_luma_coding_block_size: u8,
    /// `log2_diff_max_min_luma_transform_block_size` (4x4 up to 32x32).
    pub(crate) log2_diff_max_min_luma_transform_block_size: u8,
    /// `max_transform_hierarchy_depth_inter`.
    pub(crate) max_transform_hierarchy_depth_inter: u8,
    /// `max_transform_hierarchy_depth_intra`.
    pub(crate) max_transform_hierarchy_depth_intra: u8,
    /// `log2_max_pic_order_cnt_lsb_minus4`: 12-bit POCs, as FFmpeg uses.
    pub(crate) log2_max_pic_order_cnt_lsb_minus4: u8,
    /// DPB size minus one: one reference frame.
    pub(crate) max_dec_pic_buffering_minus1: u32,
    /// `max_num_reorder_pics`: no reordering, the stream is low-delay.
    pub(crate) max_num_reorder_pics: u32,
    /// The PPS's `init_qp_minus26`.
    pub(crate) init_qp_minus26: i32,
    /// Whether the PPS enables per-coding-unit QP deltas.
    ///
    /// FFmpeg enables them for every rate control mode but CQP; the picture parameter
    /// buffer carries the same flag, so the driver and the PPS agree.
    pub(crate) cu_qp_delta_enabled: bool,
}

impl HevcSettings {
    /// Derives the settings for one pipeline configuration.
    pub(crate) fn new(width: u32, height: u32, fps: u32, cu_qp_delta_enabled: bool) -> Self {
        let surface_width = width.max(16).div_ceil(16) * 16;
        let surface_height = height.max(16).div_ceil(16) * 16;
        Self {
            width,
            height,
            fps: fps.max(1),
            general_profile_idc: 1,
            general_level_idc: hevc_level_idc(
                u64::from(surface_width) * u64::from(surface_height),
                fps,
            ),
            general_tier_flag: 0,
            surface_width,
            surface_height,
            // 8x8..32x32 coding units: the widest range a driver is likely to use, so a
            // driver that codes 8x8 blocks still produces a conformant stream.
            log2_min_luma_coding_block_size_minus3: 0,
            log2_diff_max_min_luma_coding_block_size: 2,
            log2_diff_max_min_luma_transform_block_size: 3,
            max_transform_hierarchy_depth_inter: 3,
            max_transform_hierarchy_depth_intra: 3,
            log2_max_pic_order_cnt_lsb_minus4: 8,
            max_dec_pic_buffering_minus1: 1,
            max_num_reorder_pics: 0,
            init_qp_minus26: 0,
            cu_qp_delta_enabled,
        }
    }

    /// The coding tree unit size in luma samples (`32` with the settings above).
    pub(crate) const fn ctu_size(&self) -> u32 {
        1 << (self.log2_min_luma_coding_block_size_minus3 + 3
            + self.log2_diff_max_min_luma_coding_block_size)
    }

    /// The number of coding tree units the surface is split into.
    pub(crate) fn ctu_count(&self) -> u32 {
        self.surface_width.div_ceil(self.ctu_size())
            * self.surface_height.div_ceil(self.ctu_size())
    }

    /// `conf_win_right_offset`, in 2-sample units.
    fn conf_win_right(&self) -> u32 {
        (self.surface_width.saturating_sub(self.width)) / 2
    }

    /// `conf_win_bottom_offset`, in 2-sample units.
    fn conf_win_bottom(&self) -> u32 {
        (self.surface_height.saturating_sub(self.height)) / 2
    }

    /// Writes `video_parameter_set_rbsp()` as an Annex B NAL unit.
    pub(crate) fn write_vps(&self) -> Vec<u8> {
        let mut rbsp = BitWriter::new();
        rbsp.put_bits(0, 4); // vps_video_parameter_set_id
        rbsp.put_bit(1); // vps_base_layer_internal_flag
        rbsp.put_bit(1); // vps_base_layer_available_flag
        rbsp.put_bits(0, 6); // vps_max_layers_minus1
        rbsp.put_bits(0, 3); // vps_max_sub_layers_minus1
        rbsp.put_bit(1); // vps_temporal_id_nesting_flag
        rbsp.put_bits(0xFFFF, 16); // vps_reserved_0xffff_16bits
        self.write_profile_tier_level(&mut rbsp);
        rbsp.put_bit(0); // vps_sub_layer_ordering_info_present_flag
        // One sub-layer, so the loop runs once.
        rbsp.put_ue(self.max_dec_pic_buffering_minus1);
        rbsp.put_ue(self.max_num_reorder_pics);
        rbsp.put_ue(0); // vps_max_latency_increase_plus1
        rbsp.put_bits(0, 6); // vps_max_layer_id
        rbsp.put_ue(0); // vps_num_layer_sets_minus1
        // Timing information is left to the SPS's VUI: fewer hand-written syntax elements
        // means fewer places for this writer to disagree with a decoder.
        rbsp.put_bit(0); // vps_timing_info_present_flag
        rbsp.put_bit(0); // vps_extension_flag
        rbsp.rbsp_trailing_bits();

        annex_b_nal_hevc(hevc_nal::VPS, &rbsp.finish())
    }

    /// Writes `seq_parameter_set_rbsp()` as an Annex B NAL unit.
    pub(crate) fn write_sps(&self) -> Vec<u8> {
        let mut rbsp = BitWriter::new();
        rbsp.put_bits(0, 4); // sps_video_parameter_set_id
        rbsp.put_bits(0, 3); // sps_max_sub_layers_minus1
        rbsp.put_bit(1); // sps_temporal_id_nesting_flag
        self.write_profile_tier_level(&mut rbsp);
        rbsp.put_ue(0); // sps_seq_parameter_set_id
        rbsp.put_ue(1); // chroma_format_idc: 4:2:0
        rbsp.put_ue(self.surface_width); // pic_width_in_luma_samples
        rbsp.put_ue(self.surface_height); // pic_height_in_luma_samples

        let cropping = self.conf_win_right() != 0 || self.conf_win_bottom() != 0;
        rbsp.put_bit(u32::from(cropping)); // conformance_window_flag
        if cropping {
            rbsp.put_ue(0); // conf_win_left_offset
            rbsp.put_ue(self.conf_win_right());
            rbsp.put_ue(0); // conf_win_top_offset
            rbsp.put_ue(self.conf_win_bottom());
        }

        rbsp.put_ue(0); // bit_depth_luma_minus8
        rbsp.put_ue(0); // bit_depth_chroma_minus8
        rbsp.put_ue(u32::from(self.log2_max_pic_order_cnt_lsb_minus4));
        rbsp.put_bit(0); // sps_sub_layer_ordering_info_present_flag
        // One sub-layer, so this loop runs once.
        rbsp.put_ue(self.max_dec_pic_buffering_minus1);
        rbsp.put_ue(self.max_num_reorder_pics);
        rbsp.put_ue(0); // sps_max_latency_increase_plus1

        rbsp.put_ue(u32::from(self.log2_min_luma_coding_block_size_minus3));
        rbsp.put_ue(u32::from(self.log2_diff_max_min_luma_coding_block_size));
        rbsp.put_ue(0); // log2_min_luma_transform_block_size_minus2: 4x4
        rbsp.put_ue(u32::from(self.log2_diff_max_min_luma_transform_block_size));
        rbsp.put_ue(u32::from(self.max_transform_hierarchy_depth_inter));
        rbsp.put_ue(u32::from(self.max_transform_hierarchy_depth_intra));
        rbsp.put_bit(0); // scaling_list_enabled_flag
        rbsp.put_bit(1); // amp_enabled_flag
        // SAO and temporal MVP are declared off: the drivers this backend targets do not
        // implement them, and a stream that announces them would make the driver emit
        // syntax it cannot fill in.
        rbsp.put_bit(0); // sample_adaptive_offset_enabled_flag
        rbsp.put_bit(0); // pcm_enabled_flag
        rbsp.put_ue(0); // num_short_term_ref_pic_sets
        rbsp.put_bit(0); // long_term_ref_pics_present_flag
        rbsp.put_bit(0); // sps_temporal_mvp_enabled_flag
        rbsp.put_bit(1); // strong_intra_smoothing_enabled_flag
        rbsp.put_bit(1); // vui_parameters_present_flag
        self.write_vui(&mut rbsp);
        rbsp.put_bit(0); // sps_extension_present_flag
        rbsp.rbsp_trailing_bits();

        annex_b_nal_hevc(hevc_nal::SPS, &rbsp.finish())
    }

    /// Writes the VUI: timing info plus the bitstream restrictions FFmpeg's own encoder
    /// declares.
    ///
    /// The other optional parts (aspect ratio, overscan, video signal type, chroma location,
    /// neutral chroma, field sequence, display window, HRD) are declared absent; the frame
    /// rate and the motion-vector bounds are what a decoder needs from here.
    fn write_vui(&self, rbsp: &mut BitWriter) {
        rbsp.put_bit(0); // aspect_ratio_info_present_flag
        rbsp.put_bit(0); // overscan_info_present_flag
        rbsp.put_bit(0); // video_signal_type_present_flag
        rbsp.put_bit(0); // chroma_loc_info_present_flag
        rbsp.put_bit(0); // neutral_chroma_indication_flag
        rbsp.put_bit(0); // field_seq_flag
        rbsp.put_bit(0); // frame_field_info_present_flag
        rbsp.put_bit(0); // default_display_window_flag
        rbsp.put_bit(1); // vui_timing_info_present_flag
        rbsp.put_bits(1, 32); // vui_num_units_in_tick
        rbsp.put_bits(self.fps, 32); // vui_time_scale
        rbsp.put_bit(1); // vui_poc_proportional_to_timing_flag
        rbsp.put_ue(0); // vui_num_ticks_poc_diff_one_minus1
        rbsp.put_bit(0); // vui_hrd_parameters_present_flag
        rbsp.put_bit(1); // bitstream_restriction_flag
        rbsp.put_bit(0); // tiles_fixed_structure_flag
        rbsp.put_bit(1); // motion_vectors_over_pic_boundaries_flag
        rbsp.put_bit(1); // restricted_ref_pic_lists_flag
        rbsp.put_ue(0); // min_spatial_segmentation_idc
        rbsp.put_ue(0); // max_bytes_per_pic_denom
        rbsp.put_ue(0); // max_bits_per_min_cu_denom
        rbsp.put_ue(15); // log2_max_mv_length_horizontal
        rbsp.put_ue(15); // log2_max_mv_length_vertical
    }

    /// Writes `pic_parameter_set_rbsp()` as an Annex B NAL unit.
    pub(crate) fn write_pps(&self) -> Vec<u8> {
        let mut rbsp = BitWriter::new();
        rbsp.put_ue(0); // pps_pic_parameter_set_id
        rbsp.put_ue(0); // pps_seq_parameter_set_id
        rbsp.put_bit(0); // dependent_slice_segments_enabled_flag
        rbsp.put_bit(0); // output_flag_present_flag
        rbsp.put_bits(0, 3); // num_extra_slice_header_bits
        rbsp.put_bit(0); // sign_data_hiding_enabled_flag
        rbsp.put_bit(0); // cabac_init_present_flag
        rbsp.put_ue(0); // num_ref_idx_l0_default_active_minus1
        rbsp.put_ue(0); // num_ref_idx_l1_default_active_minus1
        rbsp.put_se(self.init_qp_minus26);
        rbsp.put_bit(0); // constrained_intra_pred_flag
        rbsp.put_bit(0); // transform_skip_enabled_flag
        rbsp.put_bit(u32::from(self.cu_qp_delta_enabled));
        if self.cu_qp_delta_enabled {
            rbsp.put_ue(0); // diff_cu_qp_delta_depth
        }
        rbsp.put_se(0); // pps_cb_qp_offset
        rbsp.put_se(0); // pps_cr_qp_offset
        rbsp.put_bit(0); // pps_slice_chroma_qp_offsets_present_flag
        rbsp.put_bit(0); // weighted_pred_flag
        rbsp.put_bit(0); // weighted_bipred_flag
        rbsp.put_bit(0); // transquant_bypass_enabled_flag
        rbsp.put_bit(0); // tiles_enabled_flag
        rbsp.put_bit(0); // entropy_coding_sync_enabled_flag
        rbsp.put_bit(1); // pps_loop_filter_across_slices_enabled_flag
        // No deblocking filter control at the slice level: the picture parameter buffer
        // declares the same, so the driver writes no deblocking syntax into slice headers.
        rbsp.put_bit(0); // deblocking_filter_control_present_flag
        rbsp.put_bit(0); // pps_scaling_list_data_present_flag
        rbsp.put_bit(0); // lists_modification_present_flag
        rbsp.put_ue(0); // log2_parallel_merge_level_minus2
        rbsp.put_bit(0); // slice_segment_header_extension_present_flag
        rbsp.put_bit(0); // pps_extension_present_flag
        rbsp.rbsp_trailing_bits();

        annex_b_nal_hevc(hevc_nal::PPS, &rbsp.finish())
    }

    /// Writes `profile_tier_level()`, which the VPS and the SPS share verbatim.
    ///
    /// The 48-bit constraint block takes its "not a range-extension profile" form: Main is
    /// not one of profiles 4..11, so the 43 bits are reserved zeros followed by the
    /// in-band-parameters flag.
    fn write_profile_tier_level(&self, rbsp: &mut BitWriter) {
        rbsp.put_bits(0, 2); // general_profile_space
        rbsp.put_bit(u32::from(self.general_tier_flag));
        rbsp.put_bits(u32::from(self.general_profile_idc), 5);
        for index in 0..32u32 {
            rbsp.put_bit(u32::from(index == u32::from(self.general_profile_idc)));
        }
        rbsp.put_bit(1); // general_progressive_source_flag
        rbsp.put_bit(0); // general_interlaced_source_flag
        rbsp.put_bit(1); // general_non_packed_constraint_flag
        rbsp.put_bit(1); // general_frame_only_constraint_flag
        rbsp.put_bits(0, 43); // reserved_zero_43bits
        rbsp.put_bit(0); // general_inbld_flag
        rbsp.put_bits(u32::from(self.general_level_idc), 8);
    }
}

/// Chooses an HEVC level from the luma sample count and the frame rate.
///
/// Same approach as the H.264 table: the first level whose picture size and luma sample
/// rate cover the stream wins, with the Main-tier bit rate column ignored (the encoder's
/// rate control keeps the stream inside whatever the server asked for).
fn hevc_level_idc(luma_samples: u64, fps: u32) -> u8 {
    /// `(level_idc, max luma picture size, max luma sample rate per second)`.
    const LEVELS: [(u8, u64, u64); 8] = [
        (120, 2_228_224, 66_846_720),
        (123, 2_228_224, 133_693_440),
        (150, 8_912_896, 267_386_880),
        (153, 8_912_896, 534_773_760),
        (156, 8_912_896, 1_069_547_520),
        (180, 35_651_584, 1_069_547_520),
        (183, 35_651_584, 2_139_095_040),
        (186, 35_651_584, 4_278_190_080),
    ];
    let per_second = luma_samples.saturating_mul(u64::from(fps.max(1)));
    for (level, max_picture_size, max_sample_rate) in LEVELS {
        if luma_samples <= max_picture_size && per_second <= max_sample_rate {
            return level;
        }
    }
    186
}

/// Wraps an H.264 RBSP into an Annex B NAL unit.
///
/// `nal_header` carries `nal_ref_idc` and `nal_unit_type`; the RBSP is escaped so the
/// result cannot contain a start code, which is what the `has_emulation_bytes` flag on the
/// packed-header buffer tells the driver.
fn annex_b_nal(nal_header: u8, rbsp: &[u8]) -> Vec<u8> {
    let mut escaped = Vec::with_capacity(rbsp.len() + 8);
    escaped.extend_from_slice(&[0, 0, 0, 1, nal_header]);
    insert_emulation_prevention(rbsp, &mut escaped);
    escaped
}

/// Wraps an H.265 RBSP into an Annex B NAL unit.
fn annex_b_nal_hevc(nal_unit_type: u8, rbsp: &[u8]) -> Vec<u8> {
    // forbidden_zero_bit (0) | nal_unit_type (6) | nuh_layer_id high bits (0),
    // then nuh_layer_id low bits (0) | nuh_temporal_id_plus1 (1).
    let header = [nal_unit_type << 1, 0x01];
    let mut escaped = Vec::with_capacity(rbsp.len() + 8);
    escaped.extend_from_slice(&[0, 0, 0, 1]);
    escaped.extend_from_slice(&header);
    insert_emulation_prevention(rbsp, &mut escaped);
    escaped
}

/// Copies `rbsp` into `out`, inserting `0x03` after any `00 00` that is followed by
/// `00`..=`03`.
///
/// This is the H.264/H.265 emulation-prevention rule: without it a payload could contain a
/// byte sequence a parser would mistake for a start code.
fn insert_emulation_prevention(rbsp: &[u8], out: &mut Vec<u8>) {
    let mut zeros = 0usize;
    for byte in rbsp {
        if zeros >= 2 && *byte <= 3 {
            out.push(0x03);
            zeros = 0;
        }
        out.push(*byte);
        if *byte == 0 {
            zeros += 1;
        } else {
            zeros = 0;
        }
    }
}

/// A bit-level RBSP writer.
///
/// Bits are packed most-significant first, the order every syntax element in these codecs
/// uses, so `put_bits` can emit a fixed-width value directly. The writer only ever grows
/// (a `Vec<u8>`) and is drained by [`BitWriter::finish`] through
/// [`BitWriter::rbsp_trailing_bits`], which pads the final byte.
struct BitWriter {
    bytes: Vec<u8>,
    /// Bits waiting in `pending`, in the low bits, most significant first.
    pending: u64,
    /// How many bits of `pending` are valid.
    pending_bits: u32,
}

impl BitWriter {
    /// A writer with no bits.
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            pending: 0,
            pending_bits: 0,
        }
    }

    /// Appends the low bit of `value`.
    fn put_bit(&mut self, value: u32) {
        self.put_bits(value & 1, 1);
    }

    /// Appends the low `count` bits of `value`, most significant first.
    fn put_bits(&mut self, value: u32, count: u32) {
        if count == 0 {
            return;
        }
        debug_assert!(count <= 32);
        let masked = if count == 32 {
            u64::from(value)
        } else {
            u64::from(value) & ((1u64 << count) - 1)
        };
        self.pending = (self.pending << count) | masked;
        self.pending_bits += count;
        while self.pending_bits >= 8 {
            let shift = self.pending_bits - 8;
            self.bytes.push(((self.pending >> shift) & 0xFF) as u8);
            self.pending_bits -= 8;
            self.pending &= (1u64 << self.pending_bits) - 1;
        }
    }

    /// Appends an unsigned Exp-Golomb code.
    fn put_ue(&mut self, value: u32) {
        let value = u64::from(value) + 1;
        // `bits` is the code length: `leading_zero_bits` minus one, because `value` always
        // has its most significant bit set.
        let bits = 64 - value.leading_zeros();
        for _ in 1..bits {
            self.put_bit(0);
        }
        // `put_bits` takes a `u32`, so the (only ever 32-bit-wide) `level_idc` case is
        // emitted in two parts rather than silently truncated.
        let split = bits.saturating_sub(32);
        if split > 0 {
            self.put_bits((value >> split) as u32, (bits - split) as u32);
            self.put_bits((value & ((1u64 << split) - 1)) as u32, split as u32);
        } else {
            self.put_bits(value as u32, bits as u32);
        }
    }

    /// Appends a signed Exp-Golomb code.
    fn put_se(&mut self, value: i32) {
        let mapped = if value <= 0 {
            // 0 -> 0, -1 -> 1, -2 -> 2, …
            value.unsigned_abs() * 2
        } else {
            // 1 -> 1, 2 -> 3, 3 -> 5, …
            value.unsigned_abs() * 2 - 1
        };
        self.put_ue(mapped);
    }

    /// Appends the `rbsp_trailing_bits()` that close every RBSP.
    fn rbsp_trailing_bits(&mut self) {
        self.put_bit(1);
        while self.pending_bits > 0 {
            self.put_bit(0);
        }
    }

    /// The finished byte string.
    fn finish(mut self) -> Vec<u8> {
        debug_assert_eq!(self.pending_bits, 0, "rbsp_trailing_bits was not called");
        // Any bits left over would be dropped: flush them padded with zeros so a caller
        // that forgot the trailing bits still gets a byte-aligned result.
        if self.pending_bits > 0 {
            let pad = 8 - self.pending_bits;
            self.put_bits(0, pad);
        }
        self.bytes
    }
}


use crate::core::codec::Codec;
use crate::core::context::decoder_stream::DecoderStream;
use crate::core::context::{type_to_linklabel, PacketBox};
use crate::core::filter::frame_pipeline_builder::FramePipelineBuilder;
use crate::core::hwaccel::HWAccelID;
use crate::core::scheduler::input_controller::SchNode;
use crate::error::{DecoderError, OpenInputError};
use crossbeam_channel::Sender;
use ffmpeg_sys_next::AVHWDeviceType::AV_HWDEVICE_TYPE_NONE;
use ffmpeg_sys_next::AVMediaType::{AVMEDIA_TYPE_AUDIO, AVMEDIA_TYPE_SUBTITLE, AVMEDIA_TYPE_VIDEO};
use ffmpeg_sys_next::AVPixelFormat::{
    AV_PIX_FMT_CUDA, AV_PIX_FMT_MEDIACODEC, AV_PIX_FMT_NONE, AV_PIX_FMT_QSV,
};
use ffmpeg_sys_next::{
    av_codec_is_decoder, av_codec_iterate, av_get_pix_fmt, av_hwdevice_find_type_by_name,
    av_hwdevice_get_type_name, avcodec_descriptor_get, avcodec_descriptor_get_by_name,
    avcodec_find_decoder, avcodec_find_decoder_by_name,
    avcodec_get_hw_config, avformat_close_input, AVCodecID, AVCodecParameters, AVFormatContext,
    AVHWDeviceType, AVMediaType, AVPixelFormat, AVERROR, AVERROR_DECODER_NOT_FOUND, EINVAL,
};
use log::{debug, error, warn};
use std::ffi::{CStr, CString};
use std::ptr::null_mut;
use std::sync::Arc;

pub(crate) struct Demuxer {
    pub(crate) url: String,
    pub(crate) is_set_read_callback: bool,
    pub(crate) in_fmt_ctx: *mut AVFormatContext,
    pub(crate) ts_offset: i64,
    pub(crate) frame_pipelines: Option<Vec<FramePipelineBuilder>>,

    pub(crate) readrate: Option<f32>,
    pub(crate) start_time_us: Option<i64>,
    pub(crate) recording_time_us: Option<i64>,
    pub(crate) exit_on_error: Option<bool>,
    pub(crate) stream_loop: Option<i32>,

    pub(crate) node: Arc<SchNode>,
    streams: Vec<DecoderStream>,
    dsts: Vec<(Sender<PacketBox>, usize, Option<usize>)>,
}

unsafe impl Send for Demuxer {}
unsafe impl Sync for Demuxer {}

impl Demuxer {
    pub(crate) fn new(
        index: usize,
        url: String,
        is_set_read_callback: bool,
        in_fmt_ctx: *mut AVFormatContext,
        ts_offset: i64,
        frame_pipelines: Option<Vec<FramePipelineBuilder>>,
        video_codec: Option<String>,
        audio_codec: Option<String>,
        subtitle_codec: Option<String>,
        readrate: Option<f32>,
        start_time_us: Option<i64>,
        recording_time_us: Option<i64>,
        exit_on_error: Option<bool>,
        stream_loop: Option<i32>,
        hwaccel: Option<String>,
        hwaccel_device: Option<String>,
        hwaccel_output_format: Option<String>,
    ) -> crate::error::Result<Self> {
        let streams = Self::init_streams(
            index,
            in_fmt_ctx,
            video_codec,
            audio_codec,
            subtitle_codec,
            hwaccel,
            hwaccel_device,
            hwaccel_output_format,
        )?;

        Ok(Self {
            url,
            is_set_read_callback,
            in_fmt_ctx,
            ts_offset,
            frame_pipelines,
            readrate,
            start_time_us,
            recording_time_us,
            exit_on_error,
            stream_loop,
            node: Arc::new(SchNode::Demux { waiter: Arc::new(Default::default()), task_exited: Arc::new(Default::default()) }),
            streams,
            dsts: vec![],
        })
    }

    fn init_streams(
        demux_index: usize,
        mut fmt_ctx: *mut AVFormatContext,
        video_codec: Option<String>,
        audio_codec: Option<String>,
        subtitle_codec: Option<String>,
        hwaccel: Option<String>,
        hwaccel_device: Option<String>,
        hwaccel_output_format: Option<String>,
    ) -> crate::error::Result<Vec<DecoderStream>> {
        unsafe {
            let stream_count = (*fmt_ctx).nb_streams;
            let mut streams = Vec::with_capacity(stream_count as usize);

            for i in 0..stream_count {
                let st = *(*fmt_ctx).streams.add(i as usize);

                let duration = (*st).duration;
                let time_base = (*st).time_base;
                let avg_framerate = (*st).avg_frame_rate;
                let codec_parameters = (*st).codecpar;
                let codec_type = (*codec_parameters).codec_type;

                let (hwaccel_id, hwaccel_device_type, hwaccel_device, hwaccel_output_format) =
                    find_hwaccel(
                        codec_type,
                        hwaccel.clone(),
                        hwaccel_device.clone(),
                        hwaccel_output_format.clone(),
                    )?;

                let codec_id = (*codec_parameters).codec_id;

                let codec_name =
                    get_codec_name(codec_type, &video_codec, &audio_codec, &subtitle_codec);
                let decoder = choose_decoder(
                    codec_name,
                    codec_type,
                    codec_parameters,
                    codec_id,
                    hwaccel_id,
                    hwaccel_device_type,
                )?;
                if decoder.is_none() {
                    avformat_close_input(&mut fmt_ctx);
                    return Err(DecoderError::NotFound.into());
                }
                let codec_desc = avcodec_descriptor_get(codec_id);

                let linklabel = type_to_linklabel(codec_type, demux_index);
                let stream = DecoderStream::new(
                    i as usize,
                    linklabel,
                    st,
                    codec_parameters,
                    codec_type,
                    decoder.unwrap().as_ptr(),
                    codec_desc,
                    duration,
                    time_base,
                    avg_framerate,
                    hwaccel_id,
                    hwaccel_device_type,
                    hwaccel_device,
                    hwaccel_output_format,
                );
                streams.push(stream);
            }

            Ok(streams)
        }
    }

    pub(crate) fn add_packet_dst(
        &mut self,
        packet_dst: Sender<PacketBox>,
        input_stream_index: usize,
        output_stream_index: usize,
    ) {
        self.dsts
            .push((packet_dst, input_stream_index, Some(output_stream_index)));
    }

    pub(crate) fn get_streams(&self) -> &Vec<DecoderStream> {
        &self.streams
    }

    pub(crate) fn get_streams_mut(&mut self) -> &mut Vec<DecoderStream> {
        &mut self.streams
    }

    pub(crate) fn get_stream_mut(&mut self, index: usize) -> &mut DecoderStream {
        &mut self.streams[index]
    }

    pub(crate) fn get_stream(&self, index: usize) -> &DecoderStream {
        &self.streams[index]
    }

    pub(crate) fn connect_stream(&mut self, index: usize) {
        if self.streams[index].is_used() {
            return;
        }
        let (sender, receiver) = crossbeam_channel::bounded(8);
        self.dsts.push((sender, index, None));
        self.streams[index].set_src(receiver);
    }

    pub(crate) fn take_dsts(&mut self) -> Vec<(Sender<PacketBox>, usize, Option<usize>)> {
        std::mem::take(&mut self.dsts)
    }

    pub(crate) fn destination_is_empty(&mut self) -> bool {
        self.dsts.is_empty()
    }
}

fn get_codec_name(
    codec_type: AVMediaType,
    video_codec: &Option<String>,
    audio_codec: &Option<String>,
    subtitle_codec: &Option<String>,
) -> Option<String> {
    if codec_type == AVMEDIA_TYPE_VIDEO {
        video_codec.clone()
    } else if codec_type == AVMEDIA_TYPE_AUDIO {
        audio_codec.clone()
    } else if codec_type == AVMEDIA_TYPE_SUBTITLE {
        subtitle_codec.clone()
    } else {
        None
    }
}

fn choose_decoder(
    codec_name: Option<String>,
    codec_type: AVMediaType,
    codec_parameters: *mut AVCodecParameters,
    codec_id: AVCodecID,
    hwaccel_id: HWAccelID,
    hwaccel_device_type: AVHWDeviceType,
) -> crate::error::Result<Option<Codec>> {
    match codec_name {
        Some(codec_name) => unsafe {
            let codec_cstr = CString::new(codec_name.clone())?;

            let mut codec = avcodec_find_decoder_by_name(codec_cstr.as_ptr());
            let desc = avcodec_descriptor_get_by_name(codec_cstr.as_ptr());

            if codec.is_null() && !desc.is_null() {
                codec = avcodec_find_decoder((*desc).id);
                if !codec.is_null() {
                    let codec_name = (*codec).name;
                    let codec_name = CStr::from_ptr(codec_name).to_str();
                    let desc_name = (*desc).name;
                    let desc_name = CStr::from_ptr(desc_name).to_str();
                    if let (Ok(codec_name), Ok(desc_name)) = (codec_name, desc_name) {
                        debug!("Matched decoder '{codec_name}' for codec '{desc_name}'.");
                    }
                }
            }

            if codec.is_null() {
                error!("Unknown decoder '{codec_name}'");
                return Err(OpenInputError::from(AVERROR_DECODER_NOT_FOUND).into());
            }

            if (*codec).type_ != codec_type {
                error!("Invalid decoder type '{codec_name}'");
                return Err(OpenInputError::InvalidArgument.into());
            }
            let codec_id = (*codec).id;

            (*codec_parameters).codec_id = codec_id;
            if (*codec_parameters).codec_type != codec_type {
                (*codec_parameters).codec_type = codec_type;
            }

            Ok(Some(Codec::new(codec)))
        },
        None => {
            if codec_type == AVMEDIA_TYPE_VIDEO
                && hwaccel_id == HWAccelID::HwaccelGeneric
                && hwaccel_device_type != AV_HWDEVICE_TYPE_NONE
            {
                let mut i = null_mut();
                loop {
                    let c = unsafe { av_codec_iterate(&mut i) };
                    if c.is_null() {
                        break;
                    }
                    unsafe {
                        if (*c).id != codec_id || av_codec_is_decoder(c) == 0 {
                            continue;
                        }
                    }

                    let mut j = 0;
                    loop {
                        unsafe {
                            let config = avcodec_get_hw_config(c, j);
                            if config.is_null() {
                                break;
                            }
                            if (*config).device_type == hwaccel_device_type {
                                let name = (*c).name;
                                let name = CStr::from_ptr(name).to_str();
                                let type_name = av_hwdevice_get_type_name(hwaccel_device_type);
                                let type_name = CStr::from_ptr(type_name).to_str();
                                if let (Ok(name), Ok(type_name)) = (name, type_name) {
                                    debug!("Selecting decoder '{name}' because of requested hwaccel method {type_name}");
                                }

                                return Ok(Some(Codec::new(c)));
                            }
                        }
                        j += 1;
                    }
                }
            }

            let c = unsafe { avcodec_find_decoder(codec_id) };
            if c.is_null() {
                Ok(None)
            } else {
                Ok(Some(Codec::new(c)))
            }
        }
    }
}

fn find_hwaccel(
    codec_type: AVMediaType,
    hwaccel: Option<String>,
    hwaccel_device: Option<String>,
    hwaccel_output_format: Option<String>,
) -> crate::error::Result<(HWAccelID, AVHWDeviceType, Option<String>, AVPixelFormat)> {
    if codec_type != AVMediaType::AVMEDIA_TYPE_VIDEO {
        return Ok((
            HWAccelID::HwaccelNone,
            AVHWDeviceType::AV_HWDEVICE_TYPE_NONE,
            None,
            AV_PIX_FMT_NONE,
        ));
    }
    let mut out_hwaccel_output_format = AV_PIX_FMT_NONE;

    match (&hwaccel, hwaccel_output_format) {
        (Some(hwaccel), None) if hwaccel == "cuvid" => {
            warn!("WARNING: Defaulting hwaccel_output_format to cuda for compatibility with older.This behavior is DEPRECATED and will be removed in the future.Please explicitly set \"hwaccel_output_format\" to \"cuda\" using the appropriate API method.");
            out_hwaccel_output_format = AV_PIX_FMT_CUDA;
        }
        (Some(hwaccel), None) if hwaccel == "qsv" => {
            warn!("WARNING: Defaulting hwaccel_output_format to qsv for compatibility with older.This behavior is DEPRECATED and will be removed in the future.Please explicitly set \"hwaccel_output_format\" to \"qsv\" using the appropriate API method.");
            out_hwaccel_output_format = AV_PIX_FMT_QSV;
        }
        (Some(hwaccel), None) if hwaccel == "mediacodec" => {
            // There is no real AVHWFrameContext implementation. Set
            // hwaccel_output_format to avoid av_hwframe_transfer_data error.
            out_hwaccel_output_format = AV_PIX_FMT_MEDIACODEC;
        }
        (_, Some(hwaccel_output_format)) => {
            let hwaccel_output_format_cstr = CString::new(hwaccel_output_format)?;

            let hwaccel_format = unsafe { av_get_pix_fmt(hwaccel_output_format_cstr.as_ptr()) };
            if hwaccel_format == AV_PIX_FMT_NONE {
                error!("Unrecognised hwaccel output format: {:?}", hwaccel_format);
            } else {
                out_hwaccel_output_format = hwaccel_format;
            }
        }
        _ => {}
    }

    let mut out_hwaccel_id = HWAccelID::HwaccelNone;
    let mut out_hwaccel_device_type = AV_HWDEVICE_TYPE_NONE;
    if let Some(mut hwaccel) = hwaccel {
        // The NVDEC hwaccels use a CUDA device, so remap the name here.
        if hwaccel == "nvdec" || hwaccel == "cuvid" {
            hwaccel = "cuda".to_string();
        }
        if hwaccel == "none" {
        } else if hwaccel == "auto" {
            out_hwaccel_id = HWAccelID::HwaccelAuto;
        } else {
            let hwaccel_cstr = CString::new(hwaccel.clone())?;
            let device_type = unsafe { av_hwdevice_find_type_by_name(hwaccel_cstr.as_ptr()) };
            if device_type != AV_HWDEVICE_TYPE_NONE {
                out_hwaccel_id = HWAccelID::HwaccelGeneric;
                out_hwaccel_device_type = device_type;
            }

            if out_hwaccel_id == HWAccelID::HwaccelNone {
                error!("Unrecognized hwaccel: {hwaccel}.");

                let mut hwaccels = Vec::new();
                loop {
                    let device_type =
                        unsafe { av_hwdevice_find_type_by_name(hwaccel_cstr.as_ptr()) };
                    if device_type == AV_HWDEVICE_TYPE_NONE {
                        break;
                    }
                    hwaccels.push(device_type);
                }
                if !hwaccels.is_empty() {
                    error!("Supported hwaccels: {:?}.", hwaccels);
                } else {
                    error!("No hardware acceleration.");
                }
                return Err(OpenInputError::from(AVERROR(EINVAL)).into());
            }
        }
    }

    Ok((
        out_hwaccel_id,
        out_hwaccel_device_type,
        hwaccel_device,
        out_hwaccel_output_format,
    ))
}

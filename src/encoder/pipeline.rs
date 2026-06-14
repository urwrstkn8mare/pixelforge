//! Depth-N encode pipelining, shared by all codecs.
//!
//! Each in-flight frame owns an [`EncodeSlot`] (its own input image, bitstream
//! buffer, encode command buffer, fence and query pool). [`EncodePipeline`]
//! rotates through the slots so that the CPU can record and submit frame N+1
//! while the GPU is still encoding frame N, instead of blocking on a fence after
//! every frame.
//!
//! The DPB images and video session are shared across slots, so encode
//! submissions must still run in DPB order on the GPU. That ordering is enforced
//! with a single timeline semaphore (each submit waits on the previous submit's
//! value and signals its own), while bitstream readback is deferred until the
//! slot is reused — keeping the encode hardware continuously fed.

use ash::vk;
use tracing::warn;

use crate::encoder::resources::{
    clear_input_image, create_bitstream_buffer, create_encode_feedback_query_pool, create_image,
    create_timeline_semaphore, map_bitstream_buffer, submit_encode_only, wait_and_read_bitstream,
    ClearImageParams,
};
use crate::encoder::{BitDepth, EncodedPacket, FrameType, PixelFormat};
use crate::error::{PixelForgeError, Result};
use crate::vulkan::VideoContext;

/// Number of encode submissions allowed to be in flight at once.
///
/// See the module docs and `docs` discussion for why 2 is the sweet spot: it
/// fully overlaps capture/convert/upload of the next frame with the GPU encode
/// of the current one, while keeping the GPU-serialized DPB chain (and end-to-end
/// latency) from growing.
pub(crate) const ENCODE_PIPELINE_DEPTH: usize = 2;

/// Per-frame packet info captured at submit time and attached to the bitstream
/// when the slot is later drained.
pub(crate) struct SlotPacketMetadata {
    pub frame_type: FrameType,
    pub is_key_frame: bool,
    pub pts: u64,
    pub dts: u64,
    /// Codec header (SPS/PPS, VPS/SPS/PPS, or AV1 sequence header) to prepend.
    /// `Some` only for frames that carry one (e.g. the IDR/key frame).
    pub header: Option<Vec<u8>>,
}

/// All per-frame resources that must be private to a single in-flight encode.
pub(crate) struct EncodeSlot {
    pub input_image: vk::Image,
    pub input_image_memory: vk::DeviceMemory,
    pub input_image_view: vk::ImageView,
    /// Tracked layout of `input_image` (to avoid UB when transitioning).
    pub input_image_layout: vk::ImageLayout,

    pub bitstream_buffer: vk::Buffer,
    pub bitstream_buffer_memory: vk::DeviceMemory,
    pub bitstream_buffer_size: usize,
    /// Persistently mapped pointer to the bitstream buffer.
    pub bitstream_buffer_ptr: *mut u8,

    pub encode_command_buffer: vk::CommandBuffer,
    pub encode_fence: vk::Fence,
    pub query_pool: vk::QueryPool,

    /// Whether an encode has been submitted to this slot and not yet drained.
    pub in_flight: bool,
    pub pending_metadata: Option<SlotPacketMetadata>,
}

/// Configuration for building an [`EncodePipeline`].
pub(crate) struct PipelineConfig<'a> {
    pub context: &'a VideoContext,
    pub aligned_width: u32,
    pub aligned_height: u32,
    pub picture_format: vk::Format,
    pub pixel_format: PixelFormat,
    pub bit_depth: BitDepth,
    pub bitstream_buffer_size: usize,
    /// Codec profile (with the codec-specific profile chained in) used for the
    /// input images, bitstream buffers and feedback query pools.
    pub profile_info: &'a vk::VideoProfileInfoKHR<'a>,
    pub command_pool: vk::CommandPool,
    /// Transfer command buffer/fence reused to zero-initialize each input image.
    pub upload_command_buffer: vk::CommandBuffer,
    pub upload_fence: vk::Fence,
}

/// Rotating set of [`EncodeSlot`]s plus the timeline semaphore that orders their
/// encode submissions.
pub(crate) struct EncodePipeline {
    slots: Vec<EncodeSlot>,
    current_slot: usize,
    /// Orders encode submissions that share DPB state.
    timeline: vk::Semaphore,
    /// Value the next submit will signal.
    next_value: u64,
    /// Value the most recent submit signaled (0 = none yet).
    last_value: u64,
}

impl EncodePipeline {
    /// Allocate the timeline semaphore and `ENCODE_PIPELINE_DEPTH` slots.
    pub(crate) fn new(config: &PipelineConfig) -> Result<Self> {
        let context = config.context;
        let device = context.device();

        let timeline = create_timeline_semaphore(context)?;

        let alloc_info = vk::CommandBufferAllocateInfo::default()
            .command_pool(config.command_pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(ENCODE_PIPELINE_DEPTH as u32);
        let command_buffers = unsafe { device.allocate_command_buffers(&alloc_info) }
            .map_err(|e| PixelForgeError::CommandBuffer(e.to_string()))?;

        let mut slots = Vec::with_capacity(ENCODE_PIPELINE_DEPTH);
        for &encode_command_buffer in &command_buffers {
            let (input_image, input_image_memory, input_image_view) = create_image(
                context,
                config.aligned_width,
                config.aligned_height,
                config.picture_format,
                false,
                config.profile_info,
            )?;

            let (bitstream_buffer, bitstream_buffer_memory) =
                create_bitstream_buffer(context, config.bitstream_buffer_size, config.profile_info)?;
            let bitstream_buffer_ptr =
                map_bitstream_buffer(context, bitstream_buffer_memory, config.bitstream_buffer_size)?;

            // Zero the padding between the user dimensions and the aligned coded
            // extent so the first frame has no undefined samples.
            clear_input_image(
                context,
                &ClearImageParams {
                    command_buffer: config.upload_command_buffer,
                    fence: config.upload_fence,
                    queue: context.transfer_queue(),
                    image: input_image,
                    width: config.aligned_width,
                    height: config.aligned_height,
                    pixel_format: config.pixel_format,
                    bit_depth: config.bit_depth,
                },
            )?;

            // Created signaled so `wait_for_encode_fences` is safe before the
            // first encode; `submit_encode_only` resets it before each submit.
            let fence_create_info =
                vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED);
            let encode_fence = unsafe { device.create_fence(&fence_create_info, None) }
                .map_err(|e| PixelForgeError::CommandBuffer(e.to_string()))?;

            let mut profile = *config.profile_info;
            let query_pool = create_encode_feedback_query_pool(context, &mut profile)?;

            slots.push(EncodeSlot {
                input_image,
                input_image_memory,
                input_image_view,
                input_image_layout: vk::ImageLayout::VIDEO_ENCODE_SRC_KHR,
                bitstream_buffer,
                bitstream_buffer_memory,
                bitstream_buffer_size: config.bitstream_buffer_size,
                bitstream_buffer_ptr,
                encode_command_buffer,
                encode_fence,
                query_pool,
                in_flight: false,
                pending_metadata: None,
            });
        }

        Ok(Self {
            slots,
            current_slot: 0,
            timeline,
            next_value: 1,
            last_value: 0,
        })
    }

    /// The slot the next frame will be encoded into.
    pub(crate) fn current(&self) -> &EncodeSlot {
        &self.slots[self.current_slot]
    }

    pub(crate) fn current_mut(&mut self) -> &mut EncodeSlot {
        &mut self.slots[self.current_slot]
    }

    /// Return the current slot's input image, first waiting for any encode still
    /// reading it to finish (so it is safe to use as a convert/upload target).
    pub(crate) fn input_image(&self, device: &ash::Device) -> vk::Image {
        if let Err(e) = self.wait_current_slot(device) {
            warn!("Failed to wait for encode input slot: {e}");
        }
        self.slots[self.current_slot].input_image
    }

    /// Wait for the current slot's in-flight encode (if any) to finish.
    pub(crate) fn wait_current_slot(&self, device: &ash::Device) -> Result<()> {
        let slot = &self.slots[self.current_slot];
        if slot.in_flight {
            unsafe { device.wait_for_fences(&[slot.encode_fence], true, u64::MAX) }
                .map_err(|e| PixelForgeError::Synchronization(e.to_string()))?;
        }
        Ok(())
    }

    /// Record the metadata for the packet that the current slot will produce.
    pub(crate) fn set_pending_metadata(&mut self, metadata: SlotPacketMetadata) {
        self.slots[self.current_slot].pending_metadata = Some(metadata);
    }

    /// Submit the current slot's recorded command buffer without waiting.
    ///
    /// Chains onto the timeline semaphore so the GPU keeps encodes in DPB order,
    /// and marks the slot in-flight. The bitstream is drained later, when the
    /// slot is reused.
    pub(crate) fn submit_current(
        &mut self,
        device: &ash::Device,
        encode_queue: vk::Queue,
    ) -> Result<()> {
        let wait = (self.last_value > 0).then_some((self.timeline, self.last_value));
        let signal_value = self.next_value;
        let slot = &self.slots[self.current_slot];

        unsafe {
            submit_encode_only(
                device,
                slot.encode_command_buffer,
                slot.encode_fence,
                encode_queue,
                wait,
                Some((self.timeline, signal_value)),
            )?;
        }

        self.last_value = signal_value;
        self.next_value = signal_value + 1;
        self.slots[self.current_slot].in_flight = true;
        Ok(())
    }

    /// Advance to the next slot after a frame has been submitted.
    pub(crate) fn advance(&mut self) {
        self.current_slot = (self.current_slot + 1) % self.slots.len();
    }

    /// Drain the current slot, returning its packet if an encode was in flight.
    pub(crate) fn drain_current(&mut self, device: &ash::Device) -> Result<Option<EncodedPacket>> {
        Self::drain_slot(device, &mut self.slots[self.current_slot])
    }

    /// Drain every in-flight slot, in submission order starting from the current
    /// one. Used to flush remaining packets at end of stream.
    pub(crate) fn flush(&mut self, device: &ash::Device) -> Result<Vec<EncodedPacket>> {
        let mut packets = Vec::new();
        let len = self.slots.len();
        for offset in 0..len {
            let index = (self.current_slot + offset) % len;
            if let Some(packet) = Self::drain_slot(device, &mut self.slots[index])? {
                packets.push(packet);
            }
        }
        Ok(packets)
    }

    /// Fences of every slot, for waiting before mutating shared session state.
    pub(crate) fn encode_fences(&self) -> Vec<vk::Fence> {
        self.slots.iter().map(|slot| slot.encode_fence).collect()
    }

    fn drain_slot(device: &ash::Device, slot: &mut EncodeSlot) -> Result<Option<EncodedPacket>> {
        if !slot.in_flight {
            return Ok(None);
        }

        let bitstream = unsafe {
            wait_and_read_bitstream(
                device,
                slot.encode_fence,
                slot.query_pool,
                slot.bitstream_buffer_ptr,
            )?
        };
        slot.in_flight = false;

        let meta = slot.pending_metadata.take().ok_or_else(|| {
            PixelForgeError::CommandBuffer(
                "Drained encode slot has bitstream data but no packet metadata".to_string(),
            )
        })?;

        let mut data = meta.header.unwrap_or_default();
        data.extend_from_slice(&bitstream);

        Ok(Some(EncodedPacket {
            data,
            frame_type: meta.frame_type,
            is_key_frame: meta.is_key_frame,
            pts: meta.pts,
            dts: meta.dts,
        }))
    }

    /// Destroy all slot resources and the timeline semaphore.
    ///
    /// # Safety
    ///
    /// All queues that may reference these resources must be idle.
    pub(crate) unsafe fn destroy(&mut self, device: &ash::Device) {
        for slot in &mut self.slots {
            if !slot.bitstream_buffer_ptr.is_null() {
                device.unmap_memory(slot.bitstream_buffer_memory);
                slot.bitstream_buffer_ptr = std::ptr::null_mut();
            }
            device.destroy_query_pool(slot.query_pool, None);
            device.destroy_fence(slot.encode_fence, None);
            device.destroy_buffer(slot.bitstream_buffer, None);
            device.free_memory(slot.bitstream_buffer_memory, None);
            device.destroy_image_view(slot.input_image_view, None);
            device.destroy_image(slot.input_image, None);
            device.free_memory(slot.input_image_memory, None);
        }
        device.destroy_semaphore(self.timeline, None);
    }
}

//! Example: Benchmark 4K GPU color conversion with Moonshine-like HDR types.
//!
//! This benchmark measures only GPU color conversion time (`ColorConverter::convert`),
//! using an RGBA16F source image and P010 output image (the same format path used
//! by Moonshine for HDR YUV420 streaming).
//!
//! Run with:
//! `cargo run --example bench_color_convert_4k --release`

use ash::vk;
use pixelforge::{
    Codec, ColorConverter, ColorConverterConfig, ColorSpace, EncodeBitDepth, EncodeConfig,
    Encoder, InputFormat, OutputFormat, PixelFormat, RateControlMode, VideoContext,
    VideoContextBuilder,
};
use std::time::{Duration, Instant};

const WIDTH: u32 = 3840;
const HEIGHT: u32 = 2160;
const WARMUP_FRAMES: usize = 120;
const MEASURE_FRAMES: usize = 600;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    println!("PixelForge 4K Color Conversion Benchmark");
    println!("resolution: {}x{}", WIDTH, HEIGHT);
    println!(
        "path: InputFormat::RGBA16F -> OutputFormat::P010 (Moonshine HDR YUV420 style)"
    );
    println!("warmup frames: {}", WARMUP_FRAMES);
    println!("measured frames: {}", MEASURE_FRAMES);

    let context = VideoContextBuilder::new()
        .app_name("Color Convert 4K Bench")
        .enable_validation(cfg!(debug_assertions))
        .build()?;

    let (codec, encoder) = create_hdr_encoder(context.clone(), WIDTH, HEIGHT)?;
    println!("target encoder codec for destination image: {:?}", codec);

    let mut converter_config =
        ColorConverterConfig::new(WIDTH, HEIGHT, InputFormat::RGBA16F, OutputFormat::P010);
    converter_config.color_space = ColorSpace::Bt2020;
    converter_config.full_range = false;

    let mut converter = ColorConverter::new(context.clone(), converter_config)?;
    let mut source = Rgba16fSourceImage::new(context, WIDTH, HEIGHT)?;

    // Upload one frame once, then repeatedly convert it to isolate conversion cost.
    let frame = generate_rgba16f_frame(WIDTH, HEIGHT);
    source.upload(&frame)?;

    for _ in 0..WARMUP_FRAMES {
        converter.convert(
            source.image(),
            vk::ImageLayout::GENERAL,
            encoder.input_image(),
        )?;
    }

    let mut samples = Vec::with_capacity(MEASURE_FRAMES);
    for _ in 0..MEASURE_FRAMES {
        let t0 = Instant::now();
        converter.convert(
            source.image(),
            vk::ImageLayout::GENERAL,
            encoder.input_image(),
        )?;
        samples.push(t0.elapsed());
    }

    print_timing_summary(&samples);

    Ok(())
}

fn create_hdr_encoder(
    context: VideoContext,
    width: u32,
    height: u32,
) -> Result<(Codec, Encoder), Box<dyn std::error::Error>> {
    let preferred_codecs = [Codec::H265, Codec::AV1, Codec::H264];
    let mut failures = Vec::new();

    for codec in preferred_codecs {
        if !context.supports_encode(codec) {
            failures.push(format!("{codec:?}: encode not supported on this GPU"));
            continue;
        }

        let cfg = match codec {
            Codec::H264 => EncodeConfig::h264(width, height),
            Codec::H265 => EncodeConfig::h265(width, height),
            Codec::AV1 => EncodeConfig::av1(width, height),
        }
        .with_pixel_format(PixelFormat::Yuv420)
        .with_bit_depth(EncodeBitDepth::Ten)
        .with_rate_control(RateControlMode::Cqp)
        .with_quality_level(28)
        .with_frame_rate(60, 1)
        .with_gop_size(60)
        .with_b_frames(0);

        match Encoder::new(context.clone(), cfg) {
            Ok(encoder) => return Ok((codec, encoder)),
            Err(e) => failures.push(format!("{codec:?}: failed to create 10-bit YUV420 encoder: {e}")),
        }
    }

    Err(format!(
        "Unable to create a 10-bit YUV420 encoder for P010 target image. Tried: {}",
        failures.join("; ")
    )
    .into())
}

fn generate_rgba16f_frame(width: u32, height: u32) -> Vec<u8> {
    // IEEE-754 half constants in little-endian u16 words.
    const F16_ZERO: u16 = 0x0000;
    const F16_HALF: u16 = 0x3800;
    const F16_ONE: u16 = 0x3C00;

    let mut words = vec![0u16; (width as usize) * (height as usize) * 4];

    for y in 0..height as usize {
        for x in 0..width as usize {
            let idx = (y * width as usize + x) * 4;
            let r = if x < (width as usize / 2) {
                F16_ONE
            } else {
                F16_ZERO
            };
            let g = if y < (height as usize / 2) {
                F16_HALF
            } else {
                F16_ONE
            };
            let b = F16_HALF;
            let a = F16_ONE;

            words[idx] = r;
            words[idx + 1] = g;
            words[idx + 2] = b;
            words[idx + 3] = a;
        }
    }

    let mut out = vec![0u8; words.len() * 2];
    for (i, v) in words.into_iter().enumerate() {
        out[i * 2] = (v & 0xFF) as u8;
        out[i * 2 + 1] = (v >> 8) as u8;
    }
    out
}

fn print_timing_summary(samples: &[Duration]) {
    if samples.is_empty() {
        println!("No timing samples collected");
        return;
    }

    let mut ms: Vec<f64> = samples.iter().map(|d| d.as_secs_f64() * 1000.0).collect();
    ms.sort_by(f64::total_cmp);

    let avg = ms.iter().sum::<f64>() / ms.len() as f64;
    let min = ms[0];
    let p50 = percentile(&ms, 0.50);
    let p95 = percentile(&ms, 0.95);
    let p99 = percentile(&ms, 0.99);
    let max = *ms.last().unwrap_or(&min);

    println!("\nGPU color conversion timing (ColorConverter::convert):");
    println!("  min:  {:8.3} ms", min);
    println!("  p50:  {:8.3} ms", p50);
    println!("  p95:  {:8.3} ms", p95);
    println!("  p99:  {:8.3} ms", p99);
    println!("  max:  {:8.3} ms", max);
    println!("  avg:  {:8.3} ms", avg);
    println!("  samples: {}", ms.len());
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    let last = sorted.len().saturating_sub(1);
    let idx = ((last as f64) * p).round() as usize;
    sorted[idx.min(last)]
}

struct Rgba16fSourceImage {
    context: VideoContext,
    image: vk::Image,
    image_layout: vk::ImageLayout,
    image_memory: vk::DeviceMemory,
    staging_buffer: vk::Buffer,
    staging_memory: vk::DeviceMemory,
    staging_size: vk::DeviceSize,
    command_pool: vk::CommandPool,
    command_buffer: vk::CommandBuffer,
    fence: vk::Fence,
    width: u32,
    height: u32,
}

impl Rgba16fSourceImage {
    fn new(context: VideoContext, width: u32, height: u32) -> Result<Self, Box<dyn std::error::Error>> {
        let device = context.device();

        let image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk::Format::R16G16B16A16_SFLOAT)
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);

        let image = unsafe { device.create_image(&image_info, None) }?;
        let image_req = unsafe { device.get_image_memory_requirements(image) };
        let image_mem_type = context
            .find_memory_type(image_req.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)
            .ok_or_else(|| "no suitable memory type for RGBA16F image".to_string())?;
        let image_alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(image_req.size)
            .memory_type_index(image_mem_type);
        let image_memory = unsafe { device.allocate_memory(&image_alloc, None) }?;
        unsafe { device.bind_image_memory(image, image_memory, 0)? };

        let staging_size = (width as vk::DeviceSize) * (height as vk::DeviceSize) * 8;
        let staging_info = vk::BufferCreateInfo::default()
            .size(staging_size)
            .usage(vk::BufferUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let staging_buffer = unsafe { device.create_buffer(&staging_info, None) }?;
        let staging_req = unsafe { device.get_buffer_memory_requirements(staging_buffer) };
        let staging_mem_type = context
            .find_memory_type(
                staging_req.memory_type_bits,
                vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
            )
            .ok_or_else(|| "no suitable memory type for staging buffer".to_string())?;
        let staging_alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(staging_req.size)
            .memory_type_index(staging_mem_type);
        let staging_memory = unsafe { device.allocate_memory(&staging_alloc, None) }?;
        unsafe { device.bind_buffer_memory(staging_buffer, staging_memory, 0)? };

        let pool_info = vk::CommandPoolCreateInfo::default()
            .queue_family_index(context.compute_queue_family())
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
        let command_pool = unsafe { device.create_command_pool(&pool_info, None) }?;

        let alloc_info = vk::CommandBufferAllocateInfo::default()
            .command_pool(command_pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        let command_buffer = unsafe { device.allocate_command_buffers(&alloc_info)? }[0];

        let fence = unsafe { device.create_fence(&vk::FenceCreateInfo::default(), None) }?;

        Ok(Self {
            context,
            image,
            image_layout: vk::ImageLayout::UNDEFINED,
            image_memory,
            staging_buffer,
            staging_memory,
            staging_size,
            command_pool,
            command_buffer,
            fence,
            width,
            height,
        })
    }

    fn image(&self) -> vk::Image {
        self.image
    }

    fn upload(&mut self, bytes: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
        if bytes.len() as vk::DeviceSize > self.staging_size {
            return Err(format!(
                "input frame too large for staging buffer: {} > {}",
                bytes.len(),
                self.staging_size
            )
            .into());
        }

        let device = self.context.device();
        let mapped = unsafe {
            device.map_memory(
                self.staging_memory,
                0,
                self.staging_size,
                vk::MemoryMapFlags::empty(),
            )?
        };

        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), mapped as *mut u8, bytes.len());
            device.unmap_memory(self.staging_memory);
        }

        unsafe {
            device.reset_fences(&[self.fence])?;
            device.reset_command_buffer(self.command_buffer, vk::CommandBufferResetFlags::empty())?;

            let begin = vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
            device.begin_command_buffer(self.command_buffer, &begin)?;

            let to_transfer = vk::ImageMemoryBarrier::default()
                .old_layout(self.image_layout)
                .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(self.image)
                .subresource_range(vk::ImageSubresourceRange {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    base_mip_level: 0,
                    level_count: 1,
                    base_array_layer: 0,
                    layer_count: 1,
                })
                .src_access_mask(vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE)
                .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE);

            device.cmd_pipeline_barrier(
                self.command_buffer,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_transfer],
            );

            let copy = vk::BufferImageCopy::default()
                .buffer_offset(0)
                .buffer_row_length(0)
                .buffer_image_height(0)
                .image_subresource(vk::ImageSubresourceLayers {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0,
                    base_array_layer: 0,
                    layer_count: 1,
                })
                .image_offset(vk::Offset3D { x: 0, y: 0, z: 0 })
                .image_extent(vk::Extent3D {
                    width: self.width,
                    height: self.height,
                    depth: 1,
                });

            device.cmd_copy_buffer_to_image(
                self.command_buffer,
                self.staging_buffer,
                self.image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &[copy],
            );

            let to_general = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .new_layout(vk::ImageLayout::GENERAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(self.image)
                .subresource_range(vk::ImageSubresourceRange {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    base_mip_level: 0,
                    level_count: 1,
                    base_array_layer: 0,
                    layer_count: 1,
                })
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE);

            device.cmd_pipeline_barrier(
                self.command_buffer,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_general],
            );

            device.end_command_buffer(self.command_buffer)?;

            let cbs = [self.command_buffer];
            let submit = vk::SubmitInfo::default().command_buffers(&cbs);
            device.queue_submit(self.context.compute_queue(), &[submit], self.fence)?;
            device.wait_for_fences(&[self.fence], true, u64::MAX)?;
        }

        self.image_layout = vk::ImageLayout::GENERAL;
        Ok(())
    }
}

impl Drop for Rgba16fSourceImage {
    fn drop(&mut self) {
        unsafe {
            let device = self.context.device();
            device.destroy_fence(self.fence, None);
            device.destroy_command_pool(self.command_pool, None);
            device.destroy_buffer(self.staging_buffer, None);
            device.free_memory(self.staging_memory, None);
            device.destroy_image(self.image, None);
            device.free_memory(self.image_memory, None);
        }
    }
}

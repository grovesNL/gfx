#![allow(missing_docs)]

use gl;
use hal::{self, command, image, memory, pso, query, ColorSlot};
use hal::buffer::IndexBufferView;
use {native as n, Backend};
use pool::{self, BufferMemory};

use std::borrow::Borrow;
use std::{mem, slice};
use std::ops::Range;
use std::sync::{Arc, Mutex};

// Command buffer implementation details:
//
// The underlying commands and data are stored inside the associated command pool.
// See the comments for further safety requirements.
// Each command buffer holds a (growable) slice of the buffers in the pool.
//
// Command buffers are recorded one-after-another for each command pool.
// Actual storage depends on the resetting behavior of the pool.

/// The place of some data in a buffer.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct BufferSlice {
    pub offset: u32,
    pub size: u32,
}

impl BufferSlice {
    fn new() -> Self {
        BufferSlice {
            offset: 0,
            size: 0,
        }
    }

    // Append a data pointer, resulting in one data pointer
    // covering the whole memory region.
    fn append(&mut self, other: BufferSlice) {
        if self.size == 0 {
            // Empty or dummy pointer
            self.offset = other.offset;
            self.size = other.size;
        } else {
            assert_eq!(self.offset + self.size, other.offset);
            self.size += other.size;
        }
    }
}

///
#[derive(Clone, Debug)]
pub enum Command {
    Dispatch(u32, u32, u32),
    DispatchIndirect(gl::types::GLuint, u64),
    Draw {
        primitive: gl::types::GLenum,
        vertices: Range<hal::VertexCount>,
        instances: Range<hal::InstanceCount>,
    },
    DrawIndexed {
        primitive: gl::types::GLenum,
        index_type: gl::types::GLenum,
        index_count: hal::IndexCount,
        index_buffer_offset: u64,
        base_vertex: hal::VertexOffset,
        instances: Range<hal::InstanceCount>,
    },
    BindIndexBuffer(gl::types::GLuint),
    BindVertexBuffer(gl::types::GLuint),
    SetViewports {
        viewport_ptr: BufferSlice,
        depth_range_ptr: BufferSlice,
    },
    SetScissors(BufferSlice),
    SetBlendColor(command::ColorValue),
    ClearColor(command::ClearColor),
    BindFrameBuffer(FrameBufferTarget, n::FrameBuffer),
    BindTargetView(FrameBufferTarget, AttachmentPoint, n::ImageView),
    SetDrawColorBuffers(usize),
    SetPatchSize(gl::types::GLint),
    BindProgram(gl::types::GLuint),
    BindBlendSlot(ColorSlot, pso::ColorBlendDesc),
    BindAttribute(n::AttributeDesc),
    UnbindAttribute(n::AttributeDesc),
}

pub type FrameBufferTarget = gl::types::GLenum;
pub type AttachmentPoint = gl::types::GLenum;

// Cache current states of the command buffer
#[derive(Clone)]
struct Cache {
    // Active primitive topology, set by the current pipeline.
    primitive: Option<gl::types::GLenum>,
    // Active index type, set by the current index buffer.
    index_type: Option<hal::IndexType>,
    // Stencil reference values (front, back).
    stencil_ref: Option<(command::StencilValue, command::StencilValue)>,
    // Blend color.
    blend_color: Option<command::ColorValue>,
    ///
    framebuffer: Option<(FrameBufferTarget, n::FrameBuffer)>,
    ///
    // Indicates that invalid commands have been recorded.
    error_state: bool,
    // Vertices per patch for tessellation primitives (patches).
    patch_size: Option<gl::types::GLint>,
    // Active program name.
    program: Option<gl::types::GLuint>,
    // Blend per attachment.
    blend_targets: Option<Vec<Option<pso::ColorBlendDesc>>>,
}

impl Cache {
    pub fn new() -> Cache {
        Cache {
            primitive: None,
            index_type: None,
            stencil_ref: None,
            blend_color: None,
            framebuffer: None,
            error_state: false,
            patch_size: None,
            program: None,
            blend_targets: None,
        }
    }
}

// This is a subset of the device limits stripped down to the ones needed
// for command buffer validation.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    max_viewports: usize,
}

impl From<hal::Limits> for Limits {
    fn from(l: hal::Limits) -> Self {
        Limits {
            max_viewports: l.max_viewports,
        }
    }
}

/// A command buffer abstraction for OpenGL.
///
/// If you want to display your rendered results to a framebuffer created externally, see the
/// `display_fb` field.
#[derive(Clone)]
pub struct RawCommandBuffer {
    pub(crate) memory: Arc<Mutex<BufferMemory>>,
    pub(crate) buf: BufferSlice,
    // Buffer id for the owning command pool.
    // Only relevant if individual resets are allowed.
    pub(crate) id: u64,
    individual_reset: bool,

    fbo: n::FrameBuffer,
    /// The framebuffer to use for rendering to the main targets (0 by default).
    ///
    /// Use this to set the framebuffer that will be used for the screen display targets created
    /// with `create_main_targets_raw`. Usually you don't need to set this field directly unless
    /// your OS doesn't provide a default framebuffer with name 0 and you have to render to a
    /// different framebuffer object that can be made visible on the screen (iOS/tvOS need this).
    ///
    /// This framebuffer must exist and be configured correctly (with renderbuffer attachments,
    /// etc.) so that rendering to it can occur immediately.
    pub display_fb: n::FrameBuffer,
    cache: Cache,
    limits: Limits,
    active_attribs: usize,
}

impl RawCommandBuffer {
    pub(crate) fn new(
        fbo: n::FrameBuffer,
        limits: Limits,
        memory: Arc<Mutex<BufferMemory>>,
    ) -> Self {
        let (id, individual_reset) = {
            let mut memory = memory
                .try_lock()
                .expect("Trying to allocate a command buffers, while memory is in-use.");

            match *memory {
                BufferMemory::Linear(_) => (0, false),
                BufferMemory::Individual { ref mut storage, ref mut next_buffer_id } => {
                    // Add a new pair of buffers
                    storage.insert(*next_buffer_id, pool::OwnedBuffer::new());
                    let id = *next_buffer_id;
                    *next_buffer_id += 1;
                    (id, true)
                }
            }
        };

        RawCommandBuffer {
            memory,
            buf: BufferSlice::new(),
            id,
            individual_reset,
            fbo,
            display_fb: 0 as n::FrameBuffer,
            cache: Cache::new(),
            limits,
            active_attribs: 0,
        }
    }

    // Soft reset only the buffers, but doesn't free any memory or clears memory
    // of the owning pool.
    pub(crate) fn soft_reset(&mut self) {
        self.buf = BufferSlice::new();
        self.cache = Cache::new();
    }

    fn push_cmd(&mut self, cmd: Command) {
        let slice = {
            let mut memory = self
                .memory
                .try_lock()
                .expect("Trying to record a command buffers, while memory is in-use.");

            let cmd_buffer = match *memory {
                BufferMemory::Linear(ref mut buffer) => &mut buffer.commands,
                BufferMemory::Individual { ref mut storage, .. } => {
                    &mut storage.get_mut(&self.id).unwrap().commands
                }
            };
            cmd_buffer.push(cmd);
            BufferSlice {
                offset: cmd_buffer.len() as u32 - 1,
                size: 1,
            }
        };
        self.buf.append(slice);
    }

    /// Copy a given vector slice into the data buffer.
    fn add<T>(&mut self, data: &[T]) -> BufferSlice {
        self.add_raw(unsafe {
            slice::from_raw_parts(
                data.as_ptr() as *const _,
                data.len() * mem::size_of::<T>(),
            )
        })
    }

    /// Copy a given u8 slice into the data buffer.
    fn add_raw(&mut self, data: &[u8]) -> BufferSlice {
        let mut memory = self
                .memory
                .try_lock()
                .expect("Trying to record a command buffers, while memory is in-use.");

        let data_buffer = match *memory {
            BufferMemory::Linear(ref mut buffer) => &mut buffer.data,
            BufferMemory::Individual { ref mut storage, .. } => {
                &mut storage.get_mut(&self.id).unwrap().data
            }
        };
        data_buffer.extend_from_slice(data);
        let slice = BufferSlice {
            offset: (data_buffer.len() - data.len()) as u32,
            size: data.len() as u32,
        };
        slice
    }

    fn update_blend_targets(&mut self, blend_targets: &Vec<pso::ColorBlendDesc>) {
        let max_blend_slots = blend_targets.len();

        if max_blend_slots > 0 {
            match self.cache.blend_targets {
                Some(ref mut cached) => {
                    if cached.len() < max_blend_slots {
                        cached.resize(max_blend_slots, None);
                    }
                }
                None => {
                    self.cache.blend_targets = Some(vec![None; max_blend_slots]);
                }
            };
        }

        for (slot, blend_target) in blend_targets.iter().enumerate() {
            let mut update_blend = false;
            if let Some(ref mut cached_targets) = self.cache.blend_targets {
                if let Some(cached_target) = cached_targets.get(slot) {
                    match cached_target {
                        &Some(ref cache) => {
                            if cache != blend_target {
                                update_blend = true;
                            }
                        }
                        &None => {
                            update_blend = true;
                        }
                    }
                }

                if update_blend {
                    cached_targets[slot] = Some(*blend_target);
                }
            }

            if update_blend {
                self.push_cmd(Command::BindBlendSlot(slot as _, *blend_target));
            }
        }
    }
}

impl command::RawCommandBuffer<Backend> for RawCommandBuffer {
    fn begin(&mut self) {
        if self.individual_reset {
            // Implicit buffer reset when individual reset is set.
            self.reset(false);
        } else {
            self.soft_reset();
        }
    }

    fn finish(&mut self) {
        // no-op
    }

    fn reset(&mut self, _release_resources: bool) {
        if !self.individual_reset {
            error!("Associated pool must allow individual resets.");
            return
        }

        self.soft_reset();
        let mut memory = self
                .memory
                .try_lock()
                .expect("Trying to reset a command buffers, while memory is in-use.");

        match *memory {
            // Linear` can't have individual reset ability.
            BufferMemory::Linear(_) => unreachable!(),
            BufferMemory::Individual { ref mut storage, .. } => {
                // TODO: should use the `release_resources` and shrink the buffers?
                storage
                    .get_mut(&self.id)
                    .map(|buffer| {
                        buffer.commands.clear();
                        buffer.data.clear();
                    });
            }
        }

    }

    fn pipeline_barrier<'a, T>(
        &mut self,
        _stages: Range<hal::pso::PipelineStage>,
        _barriers: T,
    ) where
        T: IntoIterator,
        T::Item: Borrow<memory::Barrier<'a, Backend>>,
    {
    }

    fn fill_buffer(&mut self, _buffer: &n::Buffer, _range: Range<u64>, _data: u32) {
        unimplemented!()
    }

    fn update_buffer(&mut self, _buffer: &n::Buffer, _offset: u64, _data: &[u8]) {
        unimplemented!()
    }

    fn begin_renderpass<T>(
        &mut self,
        _render_pass: &n::RenderPass,
        _frame_buffer: &n::FrameBuffer,
        _render_area: command::Rect,
        clear_values: T,
        _first_subpass: command::SubpassContents,
    ) where
        T: IntoIterator,
        T::Item: Borrow<command::ClearValue>,
    {
        for clear_value in clear_values.into_iter().map(|cv| *cv.borrow()) {
            match clear_value {
                command::ClearValue::Color(value) => {
                    self.push_cmd(Command::ClearColor(value));
                }
                command::ClearValue::DepthStencil(_) => {
                    unimplemented!();
                }
            }
        }
    }

    fn next_subpass(&mut self, _contents: command::SubpassContents) {
        unimplemented!()
    }

    fn end_renderpass(&mut self) {
    }

    fn clear_color_image(
        &mut self,
        image: &n::Image,
        _: image::ImageLayout,
        _range: image::SubresourceRange,
        value: command::ClearColor,
    ) {
        let fbo = self.fbo;
        let view = match *image {
            n::Image::Surface(id) => n::ImageView::Surface(id),
            n::Image::Texture(id) => n::ImageView::Texture(id, 0), //TODO
        };
        self.push_cmd(Command::BindFrameBuffer(gl::DRAW_FRAMEBUFFER, fbo));
        self.push_cmd(Command::BindTargetView(gl::DRAW_FRAMEBUFFER, gl::COLOR_ATTACHMENT0, view));
        self.push_cmd(Command::SetDrawColorBuffers(1));
        self.push_cmd(Command::ClearColor(value));
    }

    fn clear_depth_stencil_image(
        &mut self,
        _image: &n::Image,
        _: image::ImageLayout,
        _range: image::SubresourceRange,
        _value: command::ClearDepthStencil,
    ) {
        unimplemented!()
    }

    fn clear_attachments<T, U>(&mut self, _: T, _: U)
    where
        T: IntoIterator,
        T::Item: Borrow<command::AttachmentClear>,
        U: IntoIterator,
        U::Item: Borrow<command::Rect>,
    {
        unimplemented!()
    }

    fn resolve_image<T>(
        &mut self,
        _src: &n::Image,
        _src_layout: image::ImageLayout,
        _dst: &n::Image,
        _dst_layout: image::ImageLayout,
        _regions: T,
    ) where
        T: IntoIterator,
        T::Item: Borrow<command::ImageResolve>,
    {
        unimplemented!()
    }

    fn bind_index_buffer(&mut self, ibv: IndexBufferView<Backend>) {
        // TODO: how can we incorporate the buffer offset?
        if ibv.offset > 0 {
            warn!("Non-zero index buffer offset currently not handled.");
        }

        self.cache.index_type = Some(ibv.index_type);
        self.push_cmd(Command::BindIndexBuffer(ibv.buffer.raw));
    }

    fn bind_vertex_buffers(&mut self, vbs: hal::pso::VertexBufferSet<Backend>) {
        for vertex_buffer in vbs.0 {
            self.push_cmd(Command::BindVertexBuffer(vertex_buffer.0.raw));
        }
    }

    fn set_viewports<T>(&mut self, viewports: T)
    where
        T: IntoIterator,
        T::Item: Borrow<command::Viewport>,
    {
        // OpenGL has two functions for setting the viewports.
        // Configuring the rectangle area and setting the depth bounds are separated.
        //
        // We try to store everything into a contiguous block of memory,
        // which allows us to avoid memory allocations when executing the commands.
        let mut viewport_ptr = BufferSlice { offset: 0, size: 0 };
        let mut depth_range_ptr = BufferSlice { offset: 0, size: 0 };

        let mut len = 0;
        for viewport in viewports {
            let viewport = viewport.borrow();
            let viewport_rect = &[viewport.rect.x as f32, viewport.rect.y as f32, viewport.rect.w as f32, viewport.rect.h as f32];
            viewport_ptr.append(self.add::<f32>(viewport_rect));
            let depth_range = &[viewport.depth.start as f64, viewport.depth.end as f64];
            depth_range_ptr.append(self.add::<f64>(depth_range));
            len += 1;
        }

        match len {
            0 => {
                error!("Number of viewports can not be zero.");
                self.cache.error_state = true;
            }
            n if n <= self.limits.max_viewports => {
                self.push_cmd(Command::SetViewports { viewport_ptr, depth_range_ptr });
            }
            _ => {
                error!("Number of viewports exceeds the number of maximum viewports");
                self.cache.error_state = true;
            }
        }
    }

    fn set_scissors<T>(&mut self, scissors: T)
    where
        T: IntoIterator,
        T::Item: Borrow<command::Rect>,
    {
        let mut scissors_ptr = BufferSlice { offset: 0, size: 0 };
        let mut len = 0;
        for scissor in scissors {
            let scissor = scissor.borrow();
            let scissor = &[scissor.x as i32, scissor.y as i32, scissor.w as i32, scissor.h as i32];
            scissors_ptr.append(self.add::<i32>(scissor));
            len += 1;
        }

        match len {
            0 => {
                error!("Number of scissors can not be zero.");
                self.cache.error_state = true;
            }
            n if n <= self.limits.max_viewports => {
                self.push_cmd(Command::SetScissors(scissors_ptr));
            }
            _ => {
                error!("Number of scissors exceeds the number of maximum viewports");
                self.cache.error_state = true;
            }
        }
    }

    fn set_stencil_reference(&mut self, front: command::StencilValue, back: command::StencilValue) {
        // Only cache the stencil references values until
        // we assembled all the pieces to set the stencil state
        // from the pipeline.
        self.cache.stencil_ref = Some((front, back));
    }

    fn set_blend_constants(&mut self, cv: command::ColorValue) {
        if self.cache.blend_color != Some(cv) {
            self.cache.blend_color = Some(cv);
            self.push_cmd(Command::SetBlendColor(cv));
        }
    }

    fn bind_graphics_pipeline(&mut self, pipeline: &n::GraphicsPipeline) {
        let &n::GraphicsPipeline {
            primitive,
            patch_size,
            program,
            ref blend_targets,
            ref attributes,
        } = pipeline;

        if self.cache.primitive != Some(primitive) {
            self.cache.primitive = Some(primitive);
        }

        if self.cache.patch_size != patch_size {
            self.cache.patch_size = patch_size;
            if let Some(size) = patch_size {
                self.push_cmd(Command::SetPatchSize(size));
            }
        }

        if self.cache.program != Some(program) {
            self.cache.program = Some(program);
            self.push_cmd(Command::BindProgram(program));
        }

        for attribute in attributes {
            self.push_cmd(Command::BindAttribute(*attribute));
        }

        self.update_blend_targets(blend_targets);
    }

    fn bind_graphics_descriptor_sets<'a, T>(
        &mut self,
        _layout: &n::PipelineLayout,
        _first_set: usize,
        _sets: T,
    ) where
        T: IntoIterator,
        T::Item: Borrow<n::DescriptorSet>,
    {
    }

    fn bind_compute_pipeline(&mut self, _pipeline: &n::ComputePipeline) {
        unimplemented!()
    }

    fn bind_compute_descriptor_sets<'a, T>(
        &mut self,
        _layout: &n::PipelineLayout,
        _first_set: usize,
        _sets: T,
    ) where
        T: IntoIterator,
        T::Item: Borrow<n::DescriptorSet>,
    {
        unimplemented!()
    }

    fn dispatch(&mut self, x: u32, y: u32, z: u32) {
        self.push_cmd(Command::Dispatch(x, y, z));
    }

    fn dispatch_indirect(&mut self, buffer: &n::Buffer, offset: u64) {
        self.push_cmd(Command::DispatchIndirect(buffer.raw, offset));
    }

    fn copy_buffer<T>(&mut self, _src: &n::Buffer, _dst: &n::Buffer, _regions: T)
    where
        T: IntoIterator,
        T::Item: Borrow<command::BufferCopy>,
    {
        unimplemented!()
    }

    fn copy_image<T>(
        &mut self,
        _src: &n::Image,
        _src_layout: image::ImageLayout,
        _dst: &n::Image,
        _dst_layout: image::ImageLayout,
        _regions: T,
    ) where
        T: IntoIterator,
        T::Item: Borrow<command::ImageCopy>,
    {
        unimplemented!()
    }

    fn copy_buffer_to_image<T>(
        &mut self,
        _src: &n::Buffer,
        _dst: &n::Image,
        _dst_layout: image::ImageLayout,
        _regions: T,
    ) where
        T: IntoIterator,
        T::Item: Borrow<command::BufferImageCopy>,
    {
        unimplemented!()
    }

    fn copy_image_to_buffer<T>(
        &mut self,
        _src: &n::Image,
        _src_layout: image::ImageLayout,
        _dst: &n::Buffer,
        _regions: T,
    ) where
        T: IntoIterator,
        T::Item: Borrow<command::BufferImageCopy>,
    {
        unimplemented!()
    }

    fn draw(
        &mut self,
        vertices: Range<hal::VertexCount>,
        instances: Range<hal::InstanceCount>,
    ) {
        match self.cache.primitive {
            Some(primitive) => {
                self.push_cmd(
                    Command::Draw {
                        primitive,
                        vertices,
                        instances,
                    }
                );
            }
            None => {
                warn!("No primitive bound. An active pipeline needs to be bound before calling `draw`.");
                self.cache.error_state = true;
            }
        }
    }

    fn draw_indexed(
        &mut self,
        indices: Range<hal::IndexCount>,
        base_vertex: hal::VertexOffset,
        instances: Range<hal::InstanceCount>,
    ) {
        let (start, index_type) = match self.cache.index_type {
            Some(hal::IndexType::U16) => (indices.start * 2, gl::UNSIGNED_SHORT),
            Some(hal::IndexType::U32) => (indices.start * 4, gl::UNSIGNED_INT),
            None => {
                warn!("No index type bound. An index buffer needs to be bound before calling `draw_indexed`.");
                self.cache.error_state = true;
                return;
            }
        };
        match self.cache.primitive {
            Some(primitive) => {
                self.push_cmd(
                    Command::DrawIndexed {
                        primitive,
                        index_type,
                        index_count: indices.end - indices.start,
                        index_buffer_offset: start as _,
                        base_vertex,
                        instances,
                    }
                );
            }
            None => {
                warn!("No primitive bound. An active pipeline needs to be bound before calling `draw_indexed`.");
                self.cache.error_state = true;
            }
        }
    }

    fn draw_indirect(
        &mut self,
        _buffer: &n::Buffer,
        _offset: u64,
        _draw_count: u32,
        _stride: u32,
    ) {
        unimplemented!()
    }

    fn draw_indexed_indirect(
        &mut self,
        _buffer: &n::Buffer,
        _offset: u64,
        _draw_count: u32,
        _stride: u32,
    ) {
        unimplemented!()
    }

    fn begin_query(
        &mut self,
        _query: query::Query<Backend>,
        _flags: query::QueryControl,
    ) {
        unimplemented!()
    }
    
    fn push_graphics_constants(
        &mut self,
        _layout: &n::PipelineLayout,
        _stages: pso::ShaderStageFlags,
        _offset: u32,
        _constants: &[u32],
    ) {
        unimplemented!()
    }

    fn end_query(
        &mut self,
        _query: query::Query<Backend>,
    ) {
        unimplemented!()
    }

    fn reset_query_pool(
        &mut self,
        _pool: &(),
        _queries: Range<query::QueryId>,
    ) {
        unimplemented!()
    }

    fn write_timestamp(
        &mut self,
        _: pso::PipelineStage,
        _: query::Query<Backend>,
    ) {
        unimplemented!()
    }
    
    fn push_compute_constants(
        &mut self,
        _layout: &n::PipelineLayout,
        _offset: u32,
        _constants: &[u32],
    ) {
        unimplemented!()
    }
}

/// A subpass command buffer abstraction for OpenGL
pub struct SubpassCommandBuffer;

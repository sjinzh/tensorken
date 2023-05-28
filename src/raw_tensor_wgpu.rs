use std::{
    borrow::Cow,
    collections::HashMap,
    fmt::{Debug, Formatter},
    sync::{Arc, Once, RwLock},
};

use bytemuck::{NoUninit, Pod};
use wgpu::util::DeviceExt;

use crate::{
    num::Num, raw_tensor::RawTensor, raw_tensor_cpu::CpuRawTensor, shape::Shape,
    shape_strider::ShapeStrider,
};

// Misc WGSL notes/tips:
// - WGSL seems to aggressively prune dead code. If you don't use a binding var in WGSL, it's like that
//   binding isn't there. If you have declared it in the rust wgpu code as a binding group, there will
//   be a failure at runtime, saying the number of bindings doesn't correspond.
// - I've been told it's a good idea to cache compute pipelines, as this avoids recompiling WGSL.

/// Size of a workgroup in X and Y dimensions. Z dimension is always 1 at the moment, it's not included.
type WorkgroupSize = (usize, usize);

/// An instantiated wgpu "device", or what I'm calling a device anyway. Not sure this corresponds to
/// conventional terminology.
/// In any case, it holds a `wgpu::Device` and a `wgpu::Queue`, and it also holds a cache of compute
/// pipelines. The latter to avoid recompiling WGSL shaders.
pub struct WgpuDevice {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipelines: RwLock<HashMap<(&'static str, WorkgroupSize), Arc<wgpu::ComputePipeline>>>,
}

impl WgpuDevice {
    const MAP_SHADER: &'static str = include_str!("shaders/map.wgsl");
    const ZIP_SHADER: &'static str = include_str!("shaders/zip.wgsl");
    const REDUCE_SHADER: &'static str = include_str!("shaders/reduce.wgsl");
    const PAD_SHADER: &'static str = include_str!("shaders/pad.wgsl");
    const FUSED_MUL_ADD_SHADER: &'static str = include_str!("shaders/fused_mul_add.wgsl");

    const REPLACE_OP_NAME: &'static str = "replace_me_with_actual_operation";
    const REPLACE_DEFAULT_NAME: &'static str = "replace_me_with_actual_default()";
    const REPLACE_UNARY_OP_DEF: &'static str =
        r"fn replace_me_with_actual_operation(in: f32) -> f32 { discard; }";
    const REPLACE_BINARY_OP_DEF: &'static str =
        r"fn replace_me_with_actual_operation(in_1: f32, in_2: f32) -> f32 { discard; }";
    const REPLACE_REDUCE_DEFAULT_DEF: &'static str =
        r"fn replace_me_with_actual_default() -> f32 { discard; }";
    const REPLACE_REDUCE_THREAD_COUNT_CONST: &'static str = r"const REDUCE_THREADS: u32 = 64u;";
    const REPLACE_REDUCE_INTERMEDIATE_BUFFER_SIZE_CONST: &'static str =
        r"const INTERMEDIATE_SIZE: u32 = 64u;";

    const REPLACE_WORKGROUP_SIZE: &'static str = "@workgroup_size(64)";

    const MAP_OPS: [&str; 3] = ["exp", "log", "id"];
    const ZIP_OPS: [&str; 6] = ["add", "sub", "mul", "div", "pow", "eq"];
    const RED_OPS: [&str; 2] = ["sum", "max"];

    pub(crate) fn new() -> Self {
        let (device, queue) = Self::get_device().unwrap();
        device.start_capture();
        Self {
            device,
            queue,
            pipelines: RwLock::new(HashMap::new()),
        }
    }

    fn memo_compute_pipeline(
        &self,
        operation: &'static str,
        module: &wgpu::ShaderModule,
        entry_point: &str,
        pipelines: &mut std::sync::RwLockWriteGuard<
            HashMap<(&str, WorkgroupSize), Arc<wgpu::ComputePipeline>>,
        >,
        workgroup_size: WorkgroupSize,
    ) {
        let compute_pipeline = Arc::new(self.device.create_compute_pipeline(
            &wgpu::ComputePipelineDescriptor {
                label: Some(operation),
                layout: None,
                module,
                entry_point,
            },
        ));
        pipelines.insert((operation, workgroup_size), compute_pipeline);
    }

    #[allow(clippy::too_many_lines)]
    fn pipeline_for(
        &self,
        operation: &'static str,
        workgroup_size: WorkgroupSize,
    ) -> Arc<wgpu::ComputePipeline> {
        let entry_point = "call";

        {
            let pipelines = self.pipelines.read().unwrap();
            if let Some(r) = pipelines.get(&(operation, workgroup_size)) {
                return Arc::clone(r);
            }
        }

        let mut pipelines = self.pipelines.write().unwrap();
        // shaders with a single operation
        if operation == "pad" {
            let shader = Self::PAD_SHADER;
            let module = &self
                .device
                .create_shader_module(wgpu::ShaderModuleDescriptor {
                    label: Some(operation),
                    source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(&shader.replace(
                        Self::REPLACE_WORKGROUP_SIZE,
                        &format!(
                            "@workgroup_size({}, {})",
                            workgroup_size.0, workgroup_size.1
                        ),
                    ))),
                });
            self.memo_compute_pipeline(
                operation,
                module,
                entry_point,
                &mut pipelines,
                workgroup_size,
            );
        }

        // shaders with a single operation
        if operation == "fused_mul_add" {
            let (shader, operation) = (Self::FUSED_MUL_ADD_SHADER, operation);
            let module = &self
                .device
                .create_shader_module(wgpu::ShaderModuleDescriptor {
                    label: Some(operation),
                    source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(&shader.replace(
                        Self::REPLACE_WORKGROUP_SIZE,
                        &format!(
                            "@workgroup_size({}, {})",
                            workgroup_size.0, workgroup_size.1,
                        ),
                    ))),
                });
            self.memo_compute_pipeline(
                operation,
                module,
                entry_point,
                &mut pipelines,
                workgroup_size,
            );
        }

        if Self::MAP_OPS.contains(&operation) {
            let shader = Self::MAP_SHADER;
            let replace_op_def = Self::REPLACE_UNARY_OP_DEF;
            let module = &self
                .device
                .create_shader_module(wgpu::ShaderModuleDescriptor {
                    label: Some(operation),
                    source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(
                        &shader
                            .replace(
                                Self::REPLACE_WORKGROUP_SIZE,
                                &format!(
                                    "@workgroup_size({}, {})",
                                    workgroup_size.0, workgroup_size.1
                                ),
                            )
                            .replace(replace_op_def, "")
                            .replace(Self::REPLACE_OP_NAME, operation),
                    )),
                });
            self.memo_compute_pipeline(
                operation,
                module,
                entry_point,
                &mut pipelines,
                workgroup_size,
            );
        }

        if Self::ZIP_OPS.contains(&operation) {
            let shader = Self::ZIP_SHADER;
            let replace_op_def = Self::REPLACE_BINARY_OP_DEF;
            let module = &self
                .device
                .create_shader_module(wgpu::ShaderModuleDescriptor {
                    label: Some(operation),
                    source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(
                        &shader
                            .replace(
                                Self::REPLACE_WORKGROUP_SIZE,
                                &format!(
                                    "@workgroup_size({}, {})",
                                    workgroup_size.0, workgroup_size.1
                                ),
                            )
                            .replace(replace_op_def, "")
                            .replace(Self::REPLACE_OP_NAME, operation),
                    )),
                });
            self.memo_compute_pipeline(
                operation,
                module,
                entry_point,
                &mut pipelines,
                workgroup_size,
            );
        }

        if Self::RED_OPS.contains(&operation) {
            let shader = Self::REDUCE_SHADER;
            for operation in Self::RED_OPS {
                let module = &self
                    .device
                    .create_shader_module(wgpu::ShaderModuleDescriptor {
                        label: Some(operation),
                        source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(
                            &shader
                                .replace(
                                    Self::REPLACE_WORKGROUP_SIZE,
                                    &format!(
                                        "@workgroup_size({}, {})",
                                        workgroup_size.0, workgroup_size.1
                                    ),
                                )
                                .replace(Self::REPLACE_BINARY_OP_DEF, "")
                                .replace(Self::REPLACE_OP_NAME, operation)
                                .replace(Self::REPLACE_REDUCE_DEFAULT_DEF, "")
                                .replace(Self::REPLACE_DEFAULT_NAME, &operation.to_uppercase())
                                .replace(
                                    Self::REPLACE_REDUCE_THREAD_COUNT_CONST,
                                    &format!("const REDUCE_THREADS: u32 = {}u;", workgroup_size.1),
                                )
                                .replace(
                                    Self::REPLACE_REDUCE_INTERMEDIATE_BUFFER_SIZE_CONST,
                                    &format!(
                                        "const INTERMEDIATE_SIZE: u32 = {}u;",
                                        workgroup_size.0 * workgroup_size.1
                                    ),
                                ),
                        )),
                    });
                self.memo_compute_pipeline(
                    operation,
                    module,
                    entry_point,
                    &mut pipelines,
                    workgroup_size,
                );
            }
        }
        Arc::clone(pipelines.get(&(operation, workgroup_size)).unwrap())
    }

    async fn get_device_async() -> Option<(wgpu::Device, wgpu::Queue)> {
        // Instantiates instance of WebGPU
        let instance = wgpu::Instance::default();

        // Use the util method to respect the env vars `WGPU_POWER_PREF` and `WGPU_ADAPTER_NAME`
        // WGPU_POWER_PREF can be set to `low` or `high` to prefer integrated or discrete GPUs respectively.
        // WGPU_ADAPTER_NAME can be set to a substring of the name of the adapter you wish to use.
        let backends = wgpu::util::backend_bits_from_env().unwrap_or_else(wgpu::Backends::all);
        // println!("backends: {backends:?}");
        // let adapters = instance.enumerate_adapters(backends);

        // for adapter in adapters {
        //     let info = adapter.get_info();
        //     println!("adapter: {:?}", info);
        // }

        let adapter = wgpu::util::initialize_adapter_from_env_or_default(&instance, backends, None)
            .await
            .expect("No suitable GPU adapters found on the system!");
        let info = adapter.get_info();
        println!(
            "Using {:#?} {} with {:#?} backend (Env vars: WGPU_POWER_PREF (low|high), WGPU_ADAPTER_NAME, WGPU_BACKEND).",
            info.device_type, info.name, info.backend
        );

        // `request_device` instantiates the feature specific connection to the GPU, defining some parameters,
        //  `features` being the available features.
        let r = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    label: None,
                    features: wgpu::Features::empty(),
                    limits: wgpu::Limits::downlevel_defaults(),
                },
                None, // Some(Path::new("./trace")),
            )
            .await
            .unwrap();
        Some(r)
    }

    fn get_device() -> Option<(wgpu::Device, wgpu::Queue)> {
        pollster::block_on(Self::get_device_async())
    }
}

/// Operations avoid copying the buffer if possible, but buffers are read-only,
/// and can be shared between multiple tensors (e.g. with different shapes).
/// As a result, buffers are reference counted. Cloning a `WgpuRawTensor` is cheap.
/// The buffer lives in GPU memory.
#[derive(Clone)]
pub struct WgpuRawTensor<'a, T> {
    buffer: Arc<wgpu::Buffer>,
    strider: ShapeStrider,
    device: &'a WgpuDevice,
    phantom: std::marker::PhantomData<T>,
}

impl<T> Debug for WgpuRawTensor<'_, T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct(format!("WgpuRawTensor<{}>", std::any::type_name::<T>()).as_str())
            .field("strider", &self.strider)
            .finish()
    }
}

impl<'a, T: NoUninit + Pod> WgpuRawTensor<'a, T> {
    /// Creates a new `WgpuRawTensor` from a shape and CPU data.
    /// The data is copied to the GPU.
    /// # Panics
    /// Panics if the shape and data size mismatch.
    pub fn new(shape: &[usize], cpu_data: &[T], device: &'a WgpuDevice) -> Self {
        assert!(
            shape.size() == cpu_data.len(),
            "Shape and data size mismatch"
        );

        let strider = ShapeStrider::contiguous(shape);
        let buffer = device
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("data buffer"),
                contents: bytemuck::cast_slice(cpu_data),
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            });

        Self {
            buffer: Arc::new(buffer),
            strider,
            device,
            phantom: std::marker::PhantomData,
        }
    }

    fn byte_size(size: usize) -> u64 {
        u64::try_from(size * std::mem::size_of::<T>()).unwrap()
    }

    /// Return a new tensor with the same buffer as this one, but
    /// with a different shape. Assumes the new shape is compatible.
    fn with_strider(&self, strider: ShapeStrider) -> Self {
        Self {
            buffer: Arc::clone(&self.buffer),
            strider,
            device: self.device,
            phantom: std::marker::PhantomData,
        }
    }

    fn device(&self) -> &wgpu::Device {
        &self.device.device
    }

    fn queue(&self) -> &wgpu::Queue {
        &self.device.queue
    }

    #[allow(dead_code)]
    fn strides(&self) -> &[usize] {
        self.strider.strides()
    }

    fn make_output_buffer(&self, size: usize) -> wgpu::Buffer {
        let size = Self::byte_size(size);
        self.device().create_buffer(&wgpu::BufferDescriptor {
            label: Some("Operation output"),
            size,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        })
    }

    fn get_bind_group_unary(
        &self,
        pipeline: &wgpu::ComputePipeline,
        output_buffer: &wgpu::Buffer,
        strides_and_shapes: &wgpu::Buffer,
    ) -> wgpu::BindGroup {
        let bind_group_layout = pipeline.get_bind_group_layout(0);
        self.device().create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: self.buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: output_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: strides_and_shapes.as_entire_binding(),
                },
            ],
        })
    }

    fn get_bind_group_zip(
        &self,
        other: &Self,
        pipeline: &wgpu::ComputePipeline,
        output_buffer: &wgpu::Buffer,
        strides_and_shapes: &wgpu::Buffer,
    ) -> wgpu::BindGroup {
        let bind_group_layout = pipeline.get_bind_group_layout(0);
        self.device().create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: self.buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: other.buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: output_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: strides_and_shapes.as_entire_binding(),
                },
            ],
        })
    }

    fn encode_and_submit(
        &self,
        compute_pipeline: &wgpu::ComputePipeline,
        bind_group: &wgpu::BindGroup,
        workgroup_count: usize,
    ) {
        let mut encoder = self
            .device()
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        {
            let mut compute_pass =
                encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: None });
            compute_pass.set_pipeline(compute_pipeline);
            compute_pass.set_bind_group(0, bind_group, &[]);
            let workgroup_count = u32::try_from(workgroup_count).unwrap();
            compute_pass.dispatch_workgroups(workgroup_count, 1, 1);
        }

        // Submit commands to the GPU, and wait synchronously for them to complete.
        // This naive approach makes CPU-GPU interaction entirely synchronous.
        // In principle, it's not necessary for correctness to wait for each command to finish.
        // The output buffer needs to be created and filled before it is used in further tensor
        // operations, but wgpu tracks such dependencies automatically, and inserts the necessary
        // barriers.
        // However, submitting a large number of compute passes without waiting for the result
        // can cause resource (sometimes memory) exhaustion, and the GPU device crashes. Without this,
        // benchmarks like matmul fail at higher sizes.
        // See https://github.com/gfx-rs/wgpu/issues/3806
        let index = self.queue().submit(Some(encoder.finish()));
        self.device()
            .poll(wgpu::Maintain::WaitForSubmissionIndex(index));
    }

    fn pipeline_for(
        &self,
        operation: &'static str,
        workgroup_size: WorkgroupSize,
    ) -> Arc<wgpu::ComputePipeline> {
        self.device.pipeline_for(operation, workgroup_size)
    }
}

impl<'a, T: Num + NoUninit + Pod> WgpuRawTensor<'a, T> {
    fn get_strides_and_shapes_buffer(
        &self,
        chunk_size: usize,
        other: Option<&ShapeStrider>,
        output_strider: &ShapeStrider,
        // reduced_strides, reduced_shape, output_shape
        reduce: Option<(&[usize], &[usize], &[usize])>,
        padding: Option<&[(usize, usize)]>,
    ) -> wgpu::Buffer {
        let mut contents = Vec::with_capacity(8 * self.shape().ndims() + 3);
        contents.push(self.shape().ndims());
        contents.push(self.strider.offset());
        if let Some(other) = other {
            contents.push(other.offset());
        }
        contents.push(chunk_size);

        contents.extend(self.strider.strides());
        if let Some(other) = other {
            contents.extend(other.strides());
        }

        contents.extend(output_strider.strides());
        contents.extend(self.shape());

        if let Some((reduced_strides, reduced_shape, output_shape)) = reduce {
            contents.extend(reduced_strides);
            contents.extend(reduced_shape);
            contents.extend(output_shape);
        }

        if let Some(padding) = padding {
            for (start, _) in padding {
                contents.push(*start);
            }
            for (_, end) in padding {
                contents.push(*end);
            }
        }

        let contents_u32 = contents
            .iter()
            .map(|x| u32::try_from(*x).unwrap())
            .collect::<Vec<u32>>();

        let buffer = self
            .device()
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                usage: wgpu::BufferUsages::STORAGE,
                label: Some("shapes and strides buffer"),
                contents: bytemuck::cast_slice(&contents_u32),
            });
        buffer
    }

    fn with_buffer_strider(&self, buffer: wgpu::Buffer, strider: ShapeStrider) -> Self {
        WgpuRawTensor {
            buffer: Arc::new(buffer),
            strider,
            device: self.device,
            phantom: self.phantom,
        }
    }

    /// Max number of threads in a workgroup, as defined by WebGPU.
    const MAX_WORKGROUP_SIZE: usize = 256;
    /// Max number of workgroups in a dispatch, as defined by WebGPU.
    const MAX_WORKGROUP_COUNT: usize = 65535;

    /// Return:
    /// - workgroup size (the number of threads in each workgroup, x and y. y is always 1.),
    /// - workgroup count (the number of workgroups to run)
    /// - chunk size (the number of elements each thread will process)
    fn counts_n_sizes(work_item_count: usize) -> ((usize, usize), usize, usize) {
        if work_item_count <= Self::MAX_WORKGROUP_SIZE {
            return ((work_item_count, 1), 1, 1);
        }

        let workgroup_count =
            (work_item_count + Self::MAX_WORKGROUP_SIZE - 1) / Self::MAX_WORKGROUP_SIZE;
        if workgroup_count <= Self::MAX_WORKGROUP_COUNT {
            return ((Self::MAX_WORKGROUP_SIZE, 1), workgroup_count, 1);
        }

        let chunk_size =
            (work_item_count + Self::MAX_WORKGROUP_COUNT - 1) / Self::MAX_WORKGROUP_COUNT;
        (
            (Self::MAX_WORKGROUP_SIZE, 1),
            Self::MAX_WORKGROUP_COUNT,
            chunk_size,
        )
    }

    /// Return a new tensor with the same shape as self, after applying f to each element.
    /// Allocates a new buffer, resulting tensor is contiguous.
    fn map(&self, operation: &'static str) -> Self {
        let output_strider = ShapeStrider::contiguous(self.shape());

        let (workgroup_size, workgroup_count, chunk_size) =
            Self::counts_n_sizes(output_strider.size());

        let output_buffer = self.make_output_buffer(output_strider.size());
        let compute_pipeline = self.pipeline_for(operation, workgroup_size);
        let strides_and_shapes =
            self.get_strides_and_shapes_buffer(chunk_size, None, &output_strider, None, None);
        let bind_group = self.get_bind_group_unary(
            compute_pipeline.as_ref(),
            &output_buffer,
            &strides_and_shapes,
        );
        self.encode_and_submit(compute_pipeline.as_ref(), &bind_group, workgroup_count);

        self.with_buffer_strider(output_buffer, output_strider)
    }

    /// Return a new tensor with the same shape as self and other, after applying f to each pair of elements.
    /// Allocates a new buffer, resulting tensor is contiguous.
    fn zip(&self, other: &Self, operation: &'static str) -> Self {
        self.strider.validate_can_zip(&other.strider).unwrap();

        let output_strider = ShapeStrider::contiguous(self.shape());

        let (workgroup_size, workgroup_count, chunk_size) =
            Self::counts_n_sizes(output_strider.size());
        let output_buffer = self.make_output_buffer(output_strider.size());
        let compute_pipeline = self.pipeline_for(operation, workgroup_size);
        let strides_and_shapes = self.get_strides_and_shapes_buffer(
            chunk_size,
            Some(&other.strider),
            &output_strider,
            None,
            None,
        );
        let bind_group = self.get_bind_group_zip(
            other,
            compute_pipeline.as_ref(),
            &output_buffer,
            &strides_and_shapes,
        );
        self.encode_and_submit(compute_pipeline.as_ref(), &bind_group, workgroup_count);

        self.with_buffer_strider(output_buffer, output_strider)
    }

    fn clamp(v: usize, min: usize, max: usize) -> usize {
        if v < min {
            min
        } else if v > max {
            max
        } else {
            v
        }
    }

    /// Return:
    /// - workgroup size (the number of threads in each workgroup, x and y.),
    /// - workgroup count (the number of workgroups to run)
    /// - chunk size (the number of elements each thread will process)
    fn counts_n_sizes_reduce(
        input_tensor_size: usize,
        output_tensor_size: usize,
    ) -> (WorkgroupSize, usize, usize) {
        if output_tensor_size <= Self::MAX_WORKGROUP_SIZE {
            // Parallelize the reduction. e.g. reducing a tensor to a single scalar.

            // First parallelize on the output size as much as possible.
            let workgroup_size_x = output_tensor_size;
            // while workgroup_size_x > output_tensor_size {
            //     workgroup_size_x /= 2;
            // }
            // Then parallelize the reduction.
            // This is the number of elements that need to be reduced to a single output element.
            let reduce_size = input_tensor_size / output_tensor_size;
            // We can only have MAX_WORKGROUP_SIZE threads total.
            let max_workgroup_size_y = Self::MAX_WORKGROUP_SIZE / workgroup_size_x;
            // Arbitrarily choose to reduce at least 64 elements per thread.
            let workgroup_size_y = Self::clamp(reduce_size / 64, 1, max_workgroup_size_y);
            let workgroup_size = (workgroup_size_x, workgroup_size_y);
            let (workgroup_count, chunk_size) = (1, 1);
            dbg!(
                output_tensor_size,
                workgroup_size,
                workgroup_count,
                chunk_size
            );
            return (workgroup_size, workgroup_count, chunk_size);
        }

        // if the output tensor is larger than the max workgroup size, we don't parallelize
        // the reduce. I.e. each thread reduces one subtensor to a single element.
        // (this will check MAX_WORKGROUP_SIZE again, but ok)
        Self::counts_n_sizes(output_tensor_size)
    }

    fn reduce(&self, axes: &[usize], operation: &'static str) -> Self {
        self.strider.validate_can_reduce(axes).unwrap();

        let (output_strider, _) = self.strider.reduce(axes);

        let mut reduced_shape = vec![1usize; self.shape().ndims()];
        for &axis in axes {
            reduced_shape[axis] = self.shape()[axis];
        }
        let reduced_strider = ShapeStrider::contiguous(&reduced_shape);

        let (workgroup_size, workgroup_count, chunk_size) =
            Self::counts_n_sizes_reduce(self.strider.size(), output_strider.size());
        let output_buffer = self.make_output_buffer(output_strider.size());
        let compute_pipeline = self.pipeline_for(operation, workgroup_size);
        let strides_and_shapes = self.get_strides_and_shapes_buffer(
            chunk_size,
            None,
            &output_strider,
            Some((
                reduced_strider.strides(),
                reduced_strider.shape(),
                output_strider.shape(),
            )),
            None,
        );
        let bind_group = self.get_bind_group_unary(
            compute_pipeline.as_ref(),
            &output_buffer,
            &strides_and_shapes,
        );
        self.encode_and_submit(compute_pipeline.as_ref(), &bind_group, workgroup_count);

        self.with_buffer_strider(output_buffer, output_strider)
    }

    fn pad(&self, padding: &[(usize, usize)]) -> Self {
        self.strider.validate_can_pad(padding).unwrap();

        let output_strider = self.strider.pad(padding);

        // unary ops can keep the same _buffer_ size, i.e. we don't expand the buffer
        let output_buffer = self.make_output_buffer(output_strider.size());

        let (workgroup_size, workgroup_count, chunk_size) =
            Self::counts_n_sizes(output_strider.size());
        let compute_pipeline = self.pipeline_for("pad", workgroup_size);
        let strides_and_shapes = self.get_strides_and_shapes_buffer(
            chunk_size,
            None,
            &output_strider,
            None,
            Some(padding),
        );
        let bind_group = self.get_bind_group_unary(
            compute_pipeline.as_ref(),
            &output_buffer,
            &strides_and_shapes,
        );
        self.encode_and_submit(compute_pipeline.as_ref(), &bind_group, workgroup_count);

        self.with_buffer_strider(output_buffer, output_strider)
    }

    fn fused_multiply_add_impl(&self, other: &Self, axes: &[usize]) -> Self {
        self.strider.validate_can_zip(&other.strider).unwrap();
        self.strider.validate_can_reduce(axes).unwrap();

        let (output_strider, _) = self.strider.reduce(axes);

        let mut reduced_shape = vec![1usize; self.shape().ndims()];
        for &axis in axes {
            reduced_shape[axis] = self.shape()[axis];
        }
        let reduced_strider = ShapeStrider::contiguous(&reduced_shape);

        let (workgroup_size, workgroup_count, chunk_size) =
            Self::counts_n_sizes(output_strider.size());
        let output_buffer = self.make_output_buffer(output_strider.size());
        let compute_pipeline = self.pipeline_for("fused_mul_add", workgroup_size);
        let strides_and_shapes = self.get_strides_and_shapes_buffer(
            chunk_size,
            Some(&other.strider),
            &output_strider,
            Some((
                reduced_strider.strides(),
                reduced_strider.shape(),
                output_strider.shape(),
            )),
            None,
        );
        let bind_group = self.get_bind_group_zip(
            other,
            compute_pipeline.as_ref(),
            &output_buffer,
            &strides_and_shapes,
        );
        self.encode_and_submit(compute_pipeline.as_ref(), &bind_group, workgroup_count);

        self.with_buffer_strider(output_buffer, output_strider)
    }

    fn contiguous(&self) -> Self {
        self.map("id")
    }

    fn ravel(&self) -> Vec<T> {
        if !self.strider.is_contiguous() {
            let t = self.contiguous();
            return t.ravel();
        }

        let size = Self::byte_size(self.shape().size());
        let offset = Self::byte_size(self.strider.offset());
        let staging_buffer = self.device().create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut encoder = self
            .device()
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        // Sets adds copy operation to command encoder.
        // Will copy data from storage buffer on GPU to staging buffer on CPU.
        encoder.copy_buffer_to_buffer(&self.buffer, offset, &staging_buffer, 0, size);

        // Submits command encoder for processing
        let index = self.queue().submit(Some(encoder.finish()));

        let buffer_slice = staging_buffer.slice(..);
        let (sender, receiver) = futures_intrusive::channel::shared::oneshot_channel();
        buffer_slice.map_async(wgpu::MapMode::Read, move |v| sender.send(v).unwrap());

        // Poll the device in a blocking manner so that our future resolves.
        // In an actual application, `device.poll(...)` should
        // be called in an event loop or on another thread.
        self.device()
            .poll(wgpu::Maintain::WaitForSubmissionIndex(index));
        let poll_result = pollster::block_on(receiver.receive());

        if let Some(Ok(())) = poll_result {
            // Gets contents of buffer
            let data = buffer_slice.get_mapped_range();
            // Since contents are got in bytes, this converts these bytes back to u32
            let result = bytemuck::cast_slice(&data).to_vec();

            drop(data);
            staging_buffer.unmap();

            result
        } else {
            panic!("failed to read buffer from GPU")
        }
    }
}

static mut WGPU_DEVICE: Option<WgpuDevice> = None;
static INIT_WGPU_DEVICE: Once = Once::new();

fn get_wgpu_device() -> &'static WgpuDevice {
    unsafe {
        INIT_WGPU_DEVICE.call_once(|| {
            WGPU_DEVICE = Some(WgpuDevice::new());
        });
        return WGPU_DEVICE.as_ref().unwrap();
    }
}

impl<T: Num + NoUninit + Pod> RawTensor for WgpuRawTensor<'_, T> {
    type Elem = T;

    fn exp(&self) -> Self {
        self.map("exp")
    }

    fn log(&self) -> Self {
        self.map("log")
    }

    fn add(&self, other: &Self) -> Self {
        self.zip(other, "add")
    }

    fn sub(&self, other: &Self) -> Self {
        self.zip(other, "sub")
    }

    fn mul(&self, other: &Self) -> Self {
        self.zip(other, "mul")
    }

    fn div(&self, other: &Self) -> Self {
        self.zip(other, "div")
    }

    fn pow(&self, other: &Self) -> Self {
        self.zip(other, "pow")
    }

    fn eq(&self, other: &Self) -> Self {
        self.zip(other, "eq")
    }

    fn sum(&self, axes: &[usize]) -> Self {
        self.reduce(axes, "sum")
    }

    fn max(&self, axes: &[usize]) -> Self {
        self.reduce(axes, "max")
    }

    fn reshape(&self, shape: &[usize]) -> Self {
        self.strider.validate_can_reshape(shape).unwrap();

        if let Ok(strider) = self.strider.reshape(shape) {
            return self.with_strider(strider);
        }
        self.contiguous().reshape(shape)
    }

    fn permute(&self, permutation: &[usize]) -> Self {
        self.strider.validate_can_permute(permutation).unwrap();

        let strider = self.strider.permute(permutation);
        self.with_strider(strider)
    }

    fn expand(&self, shape: &[usize]) -> Self {
        self.strider.validate_can_expand(shape).unwrap();

        let strider = self.strider.expand(shape).unwrap();
        self.with_strider(strider)
    }

    fn pad(&self, padding: &[(usize, usize)]) -> Self {
        self.strider.validate_can_pad(padding).unwrap();

        self.pad(padding)
    }

    fn crop(&self, limits: &[(usize, usize)]) -> Self {
        self.strider.validate_can_crop(limits).unwrap();

        let strider = self.strider.crop(limits);
        self.with_strider(strider)
    }

    fn new(shape: &[usize], data: &[Self::Elem]) -> Self {
        Self::new(shape, data, get_wgpu_device())
    }

    fn shape(&self) -> &[usize] {
        self.strider.shape()
    }

    fn ravel(&self) -> Vec<Self::Elem> {
        self.ravel()
    }

    fn to_cpu(&self) -> crate::raw_tensor_cpu::CpuRawTensor<Self::Elem> {
        CpuRawTensor::new_into(self.shape(), self.ravel())
    }

    fn fused_multiply_add(&self, other: &Self, axes: &[usize]) -> Self {
        self.fused_multiply_add_impl(other, axes)
    }
}

#[cfg(test)]
mod tests {

    use std::{f32::consts::E, iter::repeat};

    use super::*;

    fn assert_vec_eq(a: &[f32], b: &[f32]) {
        assert!(
            a.iter()
                .zip(b.iter())
                .all(|(a, b)| (a.is_nan() && b.is_nan()) || (a - b).abs() < 1e-2),
            "\r\nleft : {a:?}\r\nright: {b:?}"
        );
    }

    #[test]
    fn test_single_unary_ops() {
        let input = vec![1.0, 2.0, 3.0, -1.0, -2.0, -3.0];
        let tensor = WgpuRawTensor::new(&[3, 2], &input, get_wgpu_device());
        let result = tensor.map("exp").ravel();
        // NOTE: there may be small differences between the GPU and CPU floating point ops.
        assert_vec_eq(
            &result,
            &input.iter().map(|x| x.exp()).collect::<Vec<f32>>(),
        );

        let result = tensor.map("log").ravel();
        assert_vec_eq(
            &result,
            &input.iter().map(|x| x.log(E)).collect::<Vec<f32>>(),
        );
    }

    #[test]
    fn test_sequential_unary_ops() {
        let input = vec![1.0, 2.0, -1.0, -2.0, 3.0, -3.0, 4.0, 5.0, 6.0];
        let tensor = WgpuRawTensor::new(&[3, 3], &input, get_wgpu_device());
        let result = tensor.map("exp").map("log").ravel();
        // NOTE: there may be small differences between the GPU and CPU floating point ops.
        assert_vec_eq(
            &result,
            &input.iter().map(|x| x.exp().log(E)).collect::<Vec<f32>>(),
        );
    }

    #[allow(clippy::float_cmp)]
    fn apply_binary_op(op: &str, a: f32, b: f32) -> f32 {
        match op {
            "add" => a + b,
            "sub" => a - b,
            "mul" => a * b,
            "div" => a / b,
            "pow" =>
            // Rust returns something for powers of negative bases,
            // which in general, for real exponents, is a complex number.
            // WebGPU does not do that - depending on the device, it seems to
            // either return NaN (NVidia) or some number (Intel).
            // We just make sure to call it only with positive numbers.
            {
                a.powf(b)
            }
            "eq" => {
                if a == b {
                    1.0
                } else {
                    0.0
                }
            }
            _ => panic!("unknown op: {op}"),
        }
    }

    #[test]
    fn test_binary_ops() {
        let i1 = vec![1.0, 2.0, 3.0, -1.0, -2.0, -3.0];
        let t1 = WgpuRawTensor::new(&[3, 2], &i1, get_wgpu_device());
        let i2 = vec![6.0, 7.0, 8.0, -6.0, -7.0, -8.0];
        let t2 = WgpuRawTensor::new(&[3, 2], &i2, get_wgpu_device());
        for op in WgpuDevice::ZIP_OPS {
            if op == "pow" {
                continue; // see separate test
            }
            let result = t1.zip(&t2, op).ravel();
            assert_vec_eq(
                &result,
                &i1.iter()
                    .zip(i2.iter())
                    .map(|(a, b)| apply_binary_op(op, *a, *b))
                    .collect::<Vec<f32>>(),
            );
        }
    }

    #[test]
    fn test_pow() {
        let i1 = vec![1.0, 2.0, 3.0, 4.0, 5.0, 8.0];
        let t1 = WgpuRawTensor::new(&[3, 2], &i1, get_wgpu_device());
        let i2 = vec![6.0, 7.0, 8.0, -2.0, -5.0, -10.0];
        let t2 = WgpuRawTensor::new(&[3, 2], &i2, get_wgpu_device());
        let op = "pow";
        let result = t1.zip(&t2, op).ravel();
        assert_vec_eq(
            &result,
            &i1.iter()
                .zip(i2.iter())
                .map(|(a, b)| apply_binary_op(op, *a, *b))
                .collect::<Vec<f32>>(),
        );
    }

    #[test]
    fn test_non_contiguous_binary_ops() {
        let t1 = WgpuRawTensor::new(
            &[3, 2],
            &[1.0, 2.0, 3.0, -1.0, -2.0, -3.0],
            get_wgpu_device(),
        );
        let t1 = t1.permute(&[1, 0]);
        let t2 = WgpuRawTensor::new(
            &[3, 2],
            &[6.0, 7.0, 8.0, -6.0, -7.0, -8.0],
            get_wgpu_device(),
        );
        let t2 = t2.reshape(&[2, 3]);
        for op in WgpuDevice::ZIP_OPS {
            if op == "pow" {
                continue; // see separate test
            }
            let actual = t1.zip(&t2, op).ravel();
            let expected = t1
                .ravel()
                .into_iter()
                .zip(t2.ravel().into_iter())
                .map(|(a, b)| apply_binary_op(op, a, b))
                .collect::<Vec<f32>>();
            assert_vec_eq(&actual, &expected);
        }
    }

    #[test]
    fn test_non_contiguous_pow() {
        let t1 = WgpuRawTensor::new(&[3, 2], &[1.0, 2.0, 3.0, 7.0, 8.0, 9.0], get_wgpu_device());
        let t1 = t1.permute(&[1, 0]);
        let t2 = WgpuRawTensor::new(
            &[3, 2],
            &[6.0, 7.0, 8.0, -6.0, -7.0, -8.0],
            get_wgpu_device(),
        );
        let t2 = t2.reshape(&[2, 3]);

        let op = "pow";
        let actual = t1.zip(&t2, op).ravel();
        let expected = t1
            .ravel()
            .into_iter()
            .zip(t2.ravel().into_iter())
            .map(|(a, b)| apply_binary_op(op, a, b))
            .collect::<Vec<f32>>();
        assert_vec_eq(&actual, &expected);
    }

    fn make_vec(len: u16) -> Vec<f32> {
        (0..len).map(f32::from).collect()
    }

    #[test]
    fn test_ravel() {
        let input = make_vec(24);
        let t = WgpuRawTensor::new(&[2, 3, 4], &input, get_wgpu_device());
        assert_eq!(t.ravel(), input);
    }

    fn test_reshape_24(orig_shape: &[usize], new_shape: &[usize], expected_strides: &[usize]) {
        let input = make_vec(24);
        let t = WgpuRawTensor::new(orig_shape, &input, get_wgpu_device());
        let t = t.reshape(new_shape);
        assert_eq!(t.shape(), new_shape);
        assert_eq!(t.strides(), expected_strides);
        assert_eq!(t.ravel(), make_vec(24));
    }

    #[test]
    fn test_reshape() {
        test_reshape_24(&[24], &[3, 2, 4], &[8, 4, 1]);
        test_reshape_24(&[2, 1, 3, 1, 4], &[2, 3, 4], &[12, 4, 1]);
        test_reshape_24(&[2, 1, 3, 1, 4], &[2, 3, 4, 1], &[12, 4, 1, 1]);
    }

    fn test_permute_24(orig_shape: &[usize], permutation: &[usize], expected_shape: &[usize]) {
        let input = make_vec(24);
        let t = WgpuRawTensor::new(orig_shape, &input, get_wgpu_device());
        let tp = t.permute(permutation);
        assert_eq!(tp.shape(), expected_shape);
        assert_ne!(tp.strides(), t.strides());

        let rev_perm = (0..permutation.len())
            .map(|i| permutation.iter().position(|&x| x == i).unwrap())
            .collect::<Vec<_>>();
        let tpp = tp.permute(&rev_perm);
        assert_eq!(tpp.shape(), orig_shape);
        assert_eq!(tpp.strides(), t.strides());
        assert_eq!(&tpp.ravel(), &t.ravel());
    }

    #[test]
    fn test_permute() {
        test_permute_24(&[6, 4], &[1, 0], &[4, 6]);
        test_permute_24(&[2, 3, 4], &[2, 0, 1], &[4, 2, 3]);
    }

    #[test]
    fn test_expand() {
        let t = WgpuRawTensor::new(&[1, 1], &[42.0], get_wgpu_device());
        let t = t.expand(&[5, 4]);

        assert_eq!(t.shape(), &[5, 4]);
        assert_eq!(t.strides(), &[0, 0]);
        assert_eq!(t.ravel(), repeat(42.0).take(20).collect::<Vec<_>>());
    }

    #[test]
    fn test_reduce_ops() {
        let t = WgpuRawTensor::new(&[2, 3], &make_vec(6), get_wgpu_device());
        let s = t.sum(&[0]);
        assert_eq!(s.shape(), &[1, 3]);
        assert_eq!(s.ravel(), vec![3.0, 5.0, 7.0]);
        let s = t.sum(&[1]);
        assert_eq!(s.shape(), &[2, 1]);
        assert_eq!(s.ravel(), vec![3.0, 12.0]);
        let s = t.sum(&[0, 1]);
        assert_eq!(s.shape(), &[1, 1]);
        assert_eq!(s.ravel(), vec![15.0]);

        let s = t.max(&[0]);
        assert_eq!(s.shape(), &[1, 3]);
        assert_eq!(s.ravel(), vec![3.0, 4.0, 5.0]);
        let s = t.max(&[1]);
        assert_eq!(s.shape(), &[2, 1]);
        assert_eq!(s.ravel(), vec![2.0, 5.0]);
        let s = t.max(&[0, 1]);
        assert_eq!(s.shape(), &[1, 1]);
        assert_eq!(s.ravel(), vec![5.0]);

        let t = WgpuRawTensor::new(&[1, 1, 1], &[1.0], get_wgpu_device()).expand(&[4, 2, 4]);
        assert_eq!(t.ravel(), vec![1.0; 32]);
        let s = t.sum(&[0]);
        assert_eq!(s.shape(), &[1, 2, 4]);
        assert_eq!(s.ravel(), vec![4.0; 8]);
    }

    #[test]
    fn test_reduce_ops_non_contiguous() {
        let t = WgpuRawTensor::new(&[2, 3], &make_vec(6), get_wgpu_device()).permute(&[1, 0]);
        let s = t.sum(&[0]);
        assert_eq!(s.shape(), &[1, 2]);
        assert_eq!(s.ravel(), vec![3.0, 12.0]);
        let s = t.sum(&[1]);
        assert_eq!(s.shape(), &[3, 1]);
        assert_eq!(s.ravel(), vec![3.0, 5.0, 7.0]);
        let s = t.sum(&[0, 1]);
        assert_eq!(s.shape(), &[1, 1]);
        assert_eq!(s.ravel(), vec![15.0]);
    }

    #[test]
    fn test_reduce_ops_parallel() {
        let t = WgpuRawTensor::new(&[2, 128], &make_vec(256), get_wgpu_device());
        let s = t.sum(&[0]);
        assert_eq!(s.shape(), &[1, 128]);
        let expected: Vec<_> = (128i16..128 + 256)
            .step_by(2)
            .map(|i| f32::try_from(i).unwrap())
            .collect();
        assert_eq!(s.ravel(), expected);

        let s = t.sum(&[1]);
        assert_eq!(s.shape(), &[2, 1]);
        assert_eq!(s.ravel(), vec![8128.0, 24512.0]);

        let s = t.sum(&[0, 1]);
        assert_eq!(s.shape(), &[1, 1]);
        assert_eq!(s.ravel(), vec![8128.0 + 24512.0]);

        // No powers of two
        let t = WgpuRawTensor::new(&[60, 60], &make_vec(3600), get_wgpu_device());
        let s = t.max(&[0]);
        assert_eq!(s.shape(), &[1, 60]);
        let expected: Vec<_> = (3540i16..3600).map(|i| f32::try_from(i).unwrap()).collect();
        assert_eq!(s.ravel(), expected);

        let s = t.max(&[1]);
        assert_eq!(s.shape(), &[60, 1]);
        let expected: Vec<_> = (59i16..3600)
            .step_by(60)
            .map(|i| f32::try_from(i).unwrap())
            .collect();
        assert_eq!(s.ravel(), expected);

        let s = t.sum(&[0, 1]);
        assert_eq!(s.shape(), &[1, 1]);
        assert_eq!(s.ravel(), vec![6_478_200.0]);
    }

    #[test]
    fn test_reduce_ops_non_contiguous_parallel() {
        let t = WgpuRawTensor::new(&[2, 128], &make_vec(256), get_wgpu_device()).permute(&[1, 0]);
        let s = t.sum(&[0]);
        assert_eq!(s.shape(), &[1, 2]);
        assert_eq!(s.ravel(), vec![8128.0, 24512.0]);

        let s = t.sum(&[1]);
        assert_eq!(s.shape(), &[128, 1]);

        let expected: Vec<_> = (128i16..128 + 256)
            .step_by(2)
            .map(|i| f32::try_from(i).unwrap())
            .collect();
        assert_eq!(s.ravel(), expected);

        let s = t.sum(&[0, 1]);
        assert_eq!(s.shape(), &[1, 1]);
        assert_eq!(s.ravel(), vec![8128.0 + 24512.0]);
    }

    #[test]
    fn test_crop() {
        let orig_shape = &[2, 3, 4];
        let t = WgpuRawTensor::new(orig_shape, &make_vec(24), get_wgpu_device());

        // crop single dimension
        let s = t.crop(&[(0, 1), (0, 3), (0, 4)]);
        assert_eq!(s.ravel(), make_vec(12));

        let s = t.crop(&[(1, 2), (0, 3), (0, 4)]);
        assert_eq!(
            s.ravel(),
            make_vec(12).iter().map(|x| x + 12.0).collect::<Vec<_>>()
        );

        // crop nothing
        let s = t.crop(&[(0, 2), (0, 3), (0, 4)]);
        assert_eq!(s.shape(), orig_shape);
        assert_eq!(s.strides(), t.strides());
        assert_eq!(s.ravel(), t.ravel());

        let s = t.crop(&[(0, 2), (0, 3), (1, 3)]);
        assert_eq!(s.shape(), &[2, 3, 2]);
        assert_eq!(s.strides(), t.strides());
        assert_eq!(
            s.ravel(),
            vec![1.0, 2.0, 5., 6., 9., 10., 13., 14., 17., 18., 21., 22.]
        );

        // keep cropping
        let s = s.crop(&[(0, 1), (1, 2), (0, 2)]);
        assert_eq!(s.shape(), &[1, 1, 2]);
        assert_eq!(s.strides(), t.strides());
        assert_eq!(s.ravel(), vec![5.0, 6.0]);

        // crop to single element
        let s = s.crop(&[(0, 1), (0, 1), (1, 2)]);
        assert_eq!(s.shape(), &[1, 1, 1]);
        assert_eq!(s.strides(), t.strides());
        assert_eq!(s.ravel(), vec![6.0]);
    }

    #[test]
    #[allow(clippy::float_cmp)]
    fn test_pad() {
        let orig_shape = &[2, 3, 4];
        let t = WgpuRawTensor::new(orig_shape, &make_vec(24), get_wgpu_device());

        // pad nothing
        let s = t.pad(&[(0, 0), (0, 0), (0, 0)]);
        assert_eq!(s.shape(), orig_shape);
        assert_eq!(s.strides(), t.strides());
        assert_eq!(s.ravel(), t.ravel());

        // pad a little
        let padding = &[(1, 1), (1, 1), (1, 1)];
        let s = t.pad(padding);
        assert_eq!(s.shape(), &[4, 5, 6]);
        assert_eq!(s.strides(), &[30, 6, 1]);
        let s_raveled = s.ravel();
        assert_eq!(s_raveled.len(), s.shape().size());
        assert_eq!(s_raveled.iter().filter(|&&x| x != 0.0).count(), 23);
        assert_eq!(s_raveled[s.strider.buffer_index(&[1, 1, 2])], 1.0);
        assert_eq!(s_raveled[s.strider.buffer_index(&[1, 1, 3])], 2.0);

        // pad a lot in one dimension
        let padding = &[(20, 0), (0, 0), (0, 0)];
        let s = t.pad(padding);
        assert_eq!(s.shape(), &[22, 3, 4]);
        assert_eq!(s.strides(), &[12, 4, 1]);
        let s_raveled = s.ravel();
        assert_eq!(s_raveled.iter().filter(|&&x| x != 0.0).count(), 23);
        assert_eq!(s_raveled[s.strider.buffer_index(&[20, 0, 1])], 1.0);
        assert_eq!(s_raveled[s.strider.buffer_index(&[20, 0, 2])], 2.0);

        // pad a lot
        let padding = &[(1, 2), (3, 4), (5, 6)];
        let s = t.pad(padding);
        assert_eq!(s.shape(), &[5, 10, 15]);
        assert_eq!(s.strides(), &[150, 15, 1]);
        let s_raveled = s.ravel();
        assert_eq!(s_raveled.iter().filter(|&&x| x != 0.0).count(), 23);
        assert_eq!(s_raveled[s.strider.buffer_index(&[1, 3, 6])], 1.0);
        assert_eq!(s_raveled[s.strider.buffer_index(&[1, 3, 7])], 2.0);
    }

    #[test]
    fn test_pad_reshape_expand_crop() {
        let dim = 2;
        let t = WgpuRawTensor::new(&[1], &[1.0], get_wgpu_device());
        let t = t
            .pad(&[(0, dim)])
            .reshape(&[1, dim + 1])
            .expand(&[dim, dim + 1])
            .reshape(&[dim * (dim + 1)])
            .crop(&[(0, dim * dim)]);
        assert_eq!(t.shape(), &[4]);
        assert_eq!(t.ravel(), vec![1.0, 0.0, 0.0, 1.0]);
    }

    #[test]
    fn test_fused_multiply_add() {
        // contiguous
        let t1 = WgpuRawTensor::new(&[2, 3], &make_vec(6), get_wgpu_device());
        let t2 = t1.add(&WgpuRawTensor::new(
            &[2, 3],
            &make_vec(6),
            get_wgpu_device(),
        ));

        let r = t1.fused_multiply_add(&t2, &[0]);
        assert_eq!(r.shape(), &[1, 3]);
        assert_eq!(r.ravel(), vec![18.0, 34.0, 58.0]);

        let r = t1.fused_multiply_add(&t2, &[1]);
        assert_eq!(r.shape(), &[2, 1]);
        assert_eq!(r.ravel(), vec![10.0, 100.0]);

        let r = t1.fused_multiply_add(&t2, &[0, 1]);
        assert_eq!(r.shape(), &[1, 1]);
        assert_eq!(r.ravel(), vec![110.0]);

        // different strides
        let t1 = WgpuRawTensor::new(&[1, 1], &[8.0], get_wgpu_device()).expand(&[2, 3]);
        let t2 = WgpuRawTensor::new(&[2, 3], &make_vec(6), get_wgpu_device());

        let r = t1.fused_multiply_add(&t2, &[0]);
        assert_eq!(r.shape(), &[1, 3]);
        assert_eq!(r.ravel(), vec![24.0, 40.0, 56.0]);

        let r = t1.fused_multiply_add(&t2, &[1]);
        assert_eq!(r.shape(), &[2, 1]);
        assert_eq!(r.ravel(), vec![24.0, 96.0]);

        let r = t1.fused_multiply_add(&t2, &[0, 1]);
        assert_eq!(r.shape(), &[1, 1]);
        assert_eq!(r.ravel(), vec![120.0]);

        // non_contiguous
        let t1 = WgpuRawTensor::new(&[2, 3], &make_vec(6), get_wgpu_device()).permute(&[1, 0]);
        let t2 =
            t1.add(&WgpuRawTensor::new(&[2, 3], &make_vec(6), get_wgpu_device()).permute(&[1, 0]));
        let r = t1.fused_multiply_add(&t2, &[0]);
        assert_eq!(r.shape(), &[1, 2]);
        assert_eq!(r.ravel(), vec![10.0, 100.0]);

        let r = t1.fused_multiply_add(&t2, &[1]);
        assert_eq!(r.shape(), &[3, 1]);
        assert_eq!(r.ravel(), vec![18.0, 34.0, 58.0]);

        let r = t1.fused_multiply_add(&t2, &[0, 1]);
        assert_eq!(r.shape(), &[1, 1]);
        assert_eq!(r.ravel(), vec![110.0]);
    }
}

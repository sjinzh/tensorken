
@group(0) @binding(0)
var<storage, read> input_0: array<f32>;

@group(0) @binding(1)
var<storage, read_write> output_0: array<f32>;

// ndims, input_offset, input_strides, output_strides, shape, padding_before, padding_after
@group(0) @binding(2)
var<storage, read> strides_and_shape: array<u32>;

const preamble: u32 = 2u;

fn input_strides(i: u32) -> u32 {
    return strides_and_shape[i + preamble];
}

fn output_strides(i: u32) -> u32 {
    return strides_and_shape[i + preamble + strides_and_shape[0] ];
}

fn shape(i: u32) -> u32 {
    return strides_and_shape[i + preamble + strides_and_shape[0] * 2u];
}

fn padding_before(i: u32) -> u32 {
    return strides_and_shape[i + preamble + strides_and_shape[0] * 3u];
}

fn padding_after(i: u32) -> u32 {
    return strides_and_shape[i + preamble + strides_and_shape[0] * 4u];
}

// Find the value for the given output index - figure out whether to pad,
// i.e. result is 0.0, or not, i.e. result is the value from the input.
fn value_for(output_i: u32) -> f32 {
    var input_i: u32 = strides_and_shape[1];
    
    for (var i: u32 = 0u; i < strides_and_shape[0]; i = i + 1u) {
        let len = shape(i) + padding_after(i) + padding_before(i);
        let stride = output_strides(i);
        let output_coord: u32 = output_i / stride % len;
        if output_coord < padding_before(i) || output_coord >= padding_before(i) + shape(i) {
            return 0.0;
        }
        let input_coord = output_coord - padding_before(i);

        input_i += input_coord * input_strides(i);
    }

    return input_0[input_i];
}

@compute @workgroup_size(64)
fn call(@builtin(global_invocation_id) global_id: vec3<u32>) {
    let gidx = global_id.x;
    // because of workgroup size, gidx is a multiple of 64. Our output array may not be,
    // so we need to make sure we don't go out of bounds. Such acesses are clamped by WGSL,
    // but will typically result in wrong results anyway.
    if(global_id.x >= arrayLength(&output_0)) {
        return;
    }

    output_0[gidx] = value_for(gidx);
}
//! Read-only, opt-in comparison of shallow-water output at dispatch and at submission landing.
use super::*;
use std::sync::atomic::AtomicU64;

pub(crate) const HEIGHT_LOOKUP: &str = "92b4790eb4c29f6d";
const LIMIT: u64 = 64 * 1024;

pub(crate) fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_WATER_TRACE").as_deref() == Ok("1"))
}

pub(crate) fn sample_frame(frame: u64) -> bool {
    static LAST: AtomicU64 = AtomicU64::new(u64::MAX);
    // At most one dispatched lookup per presented frame, even on multiple recording threads.
    LAST.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |old| {
        (old == u64::MAX || old < frame).then_some(frame)
    })
    .is_ok()
}

pub(super) struct Probe {
    back: ID3D12Resource,
    _source: ID3D12Resource,
    address: u64,
    bytes: usize,
    frame: u64,
    groups: Option<[u32; 3]>,
}

pub(crate) fn record(
    list: &HostList,
    shadow: &crate::shadow::Shadow,
    address: u64,
    bytes: u64,
    frame: u64,
    groups: Option<[u32; 3]>,
) {
    let bytes = bytes.min(shadow.left(address)).min(LIMIT) & !3;
    if bytes == 0 {
        return;
    }
    let Some(host) = host() else { return };
    if let Some(probe) = Probe::snapshot(
        host,
        list,
        &shadow.buffer,
        shadow.offset(address),
        address,
        bytes,
        frame,
        groups,
    ) {
        list.deferred().water_probes.push(probe);
    }
}

impl Probe {
    fn snapshot(
        host: &Host,
        list: &HostList,
        buffer: &ID3D12Resource,
        offset: u64,
        address: u64,
        bytes: u64,
        frame: u64,
        groups: Option<[u32; 3]>,
    ) -> Option<Self> {
        let back = host.read_back_buffer(bytes)?;
        let uav = D3D12_RESOURCE_STATE_UNORDERED_ACCESS;
        let source = D3D12_RESOURCE_STATE_COPY_SOURCE;
        // SAFETY: the immediately preceding dispatch used this UAV. Restore the state,
        // and retain both resources with the recording until the list can be reset.
        unsafe {
            list.list
                .ResourceBarrier(&[transition(buffer, uav, source)]);
            list.list.CopyBufferRegion(&back, 0, buffer, offset, bytes);
            list.list
                .ResourceBarrier(&[transition(buffer, source, uav)]);
        }
        Some(Self {
            back,
            _source: buffer.clone(),
            address,
            bytes: bytes as usize,
            frame,
            groups,
        })
    }

    fn gpu_words(&self) -> Option<Vec<u32>> {
        let range = D3D12_RANGE {
            Begin: 0,
            End: self.bytes,
        };
        let mut data = std::ptr::null_mut();
        // SAFETY: only called after the submission fence. The allocation contains
        // self.bytes bytes and is mapped at a dword-aligned start.
        unsafe {
            self.back.Map(0, Some(&range), Some(&mut data)).ok()?;
            let words = std::slice::from_raw_parts(data.cast::<u32>(), self.bytes / 4).to_vec();
            self.back.Unmap(0, Some(&D3D12_RANGE { Begin: 0, End: 0 }));
            Some(words)
        }
    }

    pub(super) fn report(&self) {
        let Some(gpu) = self.gpu_words() else {
            tracing::warn!("water lookup readback map failed");
            return;
        };
        let mut title = vec![0u8; self.bytes];
        let available = read_into(self.address as *const u8, &mut title);
        let mut differences = 0usize;
        let mut examples = Vec::new();
        let mut sentinels = 0usize;
        let mut zeros = 0usize;
        for (i, (&bits, word)) in gpu.iter().zip(title.chunks_exact(4)).enumerate() {
            let host = u32::from_le_bytes(word.try_into().unwrap());
            let unequal = available && bits != host;
            sentinels += usize::from(bits == (-10000.0f32).to_bits());
            zeros += usize::from(f32::from_bits(bits) == 0.0);
            if i < 4 || (unequal && examples.len() < 12) {
                examples.push((
                    i,
                    f32::from_bits(bits),
                    available.then(|| f32::from_bits(host)),
                ));
            }
            differences += usize::from(unequal);
        }
        tracing::info!(frame=self.frame, groups=?self.groups, address=format_args!("{:#x}",self.address),
            words=gpu.len(), zeros, sentinels, title_readable=available, differences=?available.then_some(differences),
            samples=?examples, "water lookup u11: dispatch result and title memory at landing");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_output_survives_probe_and_resumed_uav_writes() {
        let host = host().expect("D3D12 device");
        let code = compile(
            r#"
            #define RS "RootConstants(num32BitConstants=2, b0), UAV(u0)"
            cbuffer Params : register(b0) { uint index; uint bits; };
            RWByteAddressBuffer dst : register(u0);
            [RootSignature(RS)] [numthreads(1, 1, 1)]
            void main() { dst.Store(index * 4, bits); }
        "#,
            windows::core::s!("main"),
            windows::core::s!("cs_5_1"),
        )
        .unwrap();
        let root: ID3D12RootSignature =
            unsafe { host.device.CreateRootSignature(0, &code).unwrap() };
        let pso: ID3D12PipelineState = unsafe {
            host.device
                .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                    pRootSignature: std::mem::transmute_copy(&root),
                    CS: D3D12_SHADER_BYTECODE {
                        pShaderBytecode: code.as_ptr().cast(),
                        BytecodeLength: code.len(),
                    },
                    ..Default::default()
                })
                .unwrap()
        };
        let buffer = host.uav_buffer(64).unwrap();
        let list = host.new_list().unwrap();
        let mut results = Vec::new();
        let expected = [(-10000.0f32).to_bits(), 3.25f32.to_bits()];
        unsafe {
            list.list.SetComputeRootSignature(&root);
            list.list.SetPipelineState(&pso);
            list.list
                .SetComputeRootUnorderedAccessView(1, buffer.GetGPUVirtualAddress());
            for value in expected {
                let constants = [3u32, value];
                list.list
                    .SetComputeRoot32BitConstants(0, 2, constants.as_ptr().cast(), 0);
                list.list.Dispatch(1, 1, 1);
                results.push(
                    Probe::snapshot(host, &list, &buffer, 12, 0, 4, 1, Some([1; 3])).unwrap(),
                );
            }
        }
        list.close();
        host.execute(&[&list]);
        for (probe, value) in results.iter().zip(expected) {
            assert_eq!(probe.gpu_words().as_deref(), Some([value].as_slice()));
        }
    }
}

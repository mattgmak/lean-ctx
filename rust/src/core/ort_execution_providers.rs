//! ONNX Runtime execution provider selection: CPU default, opt-in GPU providers.
//!
//! Each GPU EP is gated behind its own Cargo feature (`ort-cuda`, `ort-rocm`, etc.).
//! `LEAN_CTX_ORT_EXECUTION_PROVIDER=cpu|gpu|auto` controls runtime selection.
//! By default, `auto` enables GPU only when the selected ORT dylib looks like a
//! GPU runtime; otherwise CPU is used. ORT falls back to CPU when a registered
//! GPU EP is unusable.

use std::path::Path;

const PROVIDER_ENV: &str = "LEAN_CTX_ORT_EXECUTION_PROVIDER";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderPolicy {
    Cpu,
    Gpu,
    Auto,
}

/// Build the execution provider list for the current runtime policy.
pub fn execution_providers() -> Vec<ort::ep::ExecutionProviderDispatch> {
    match provider_policy() {
        ProviderPolicy::Cpu => cpu_execution_providers(),
        ProviderPolicy::Gpu => gpu_execution_providers(),
        ProviderPolicy::Auto => {
            if selected_runtime_looks_gpu() {
                gpu_execution_providers()
            } else {
                tracing::debug!(
                    env = PROVIDER_ENV,
                    "ONNX Runtime GPU auto-detect did not find a GPU runtime; using CPU"
                );
                cpu_execution_providers()
            }
        }
    }
}

pub fn execution_provider_status() -> String {
    let policy = provider_policy_name();
    let compiled = compiled_gpu_provider_names();
    let compiled = if compiled.is_empty() {
        "none".to_string()
    } else {
        compiled.join(",")
    };
    format!(
        "ORT execution provider policy: {policy} (env {PROVIDER_ENV}; compiled GPU EPs: {compiled})"
    )
}

/// Whether the current policy resolves to a GPU execution provider — regardless
/// of whether that provider's runtime dependencies can actually be loaded.
fn policy_wants_gpu() -> bool {
    if compiled_gpu_provider_names().is_empty() {
        return false;
    }
    match provider_policy() {
        ProviderPolicy::Cpu => false,
        ProviderPolicy::Gpu => true,
        ProviderPolicy::Auto => selected_runtime_looks_gpu(),
    }
}

/// Whether a real GPU execution provider will *actually* run inference — i.e.
/// the policy wants a GPU **and** the provider's runtime libraries load. Used to
/// scale batch size: small mini-batches under-utilize a GPU and pay
/// kernel-launch/host↔device-copy overhead per call that isn't amortized
/// (notably under WSL2 GPU passthrough), but oversizing batches for a GPU that
/// silently fell back to CPU makes the CPU path dramatically slower — so this
/// must reflect the EP that ORT will really register, not just the policy.
pub fn gpu_active() -> bool {
    if !policy_wants_gpu() {
        return false;
    }
    // The shipped Linux/Windows GPU build compiles only the CUDA EP. If its
    // runtime deps (libcudart/libcublas/libcudnn/…) can't be dlopen'd, ORT
    // silently registers CPU instead; don't size batches for a phantom GPU.
    #[cfg(feature = "ort-cuda")]
    {
        cuda_runtime_available()
    }
    #[cfg(not(feature = "ort-cuda"))]
    {
        true
    }
}

/// When the policy expects a GPU but the CUDA runtime can't be loaded (so ORT
/// falls back to CPU), returns a user-facing explanation with the exact install
/// commands for the missing libraries. Returns `None` when the GPU actually
/// works or when CPU was requested.
pub fn gpu_fallback_warning() -> Option<String> {
    #[cfg(feature = "ort-cuda")]
    {
        if !policy_wants_gpu() || cuda_runtime_available() {
            return None;
        }
        let detail = probe_cuda_runtime().err().unwrap_or_default();
        Some(cuda_missing_message(&detail))
    }
    #[cfg(not(feature = "ort-cuda"))]
    {
        None
    }
}

/// Filename of the ORT CUDA provider shared library for the current platform.
#[cfg(feature = "ort-cuda")]
fn cuda_provider_lib_name() -> &'static str {
    #[cfg(target_os = "windows")]
    {
        "onnxruntime_providers_cuda.dll"
    }
    #[cfg(target_os = "macos")]
    {
        "libonnxruntime_providers_cuda.dylib"
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        "libonnxruntime_providers_cuda.so"
    }
}

/// Path to the ORT CUDA provider library, resolved next to the selected ORT dylib.
#[cfg(feature = "ort-cuda")]
fn cuda_provider_lib_path() -> Option<std::path::PathBuf> {
    let dylib = crate::core::ort_environment::resolved_ort_dylib_path().ok()?;
    Some(dylib.parent()?.join(cuda_provider_lib_name()))
}

/// Probe whether the CUDA provider library and its transitive CUDA runtime
/// dependencies can be loaded — the same resolution ORT performs when it
/// registers the EP. `Err` carries the loader message (e.g. the first missing
/// `.so`), which surfaces in the fallback warning.
#[cfg(feature = "ort-cuda")]
fn probe_cuda_runtime() -> Result<(), String> {
    let path = cuda_provider_lib_path()
        .ok_or_else(|| "could not resolve the ORT CUDA provider library path".to_string())?;
    if !path.exists() {
        return Err(format!("{} not found", path.display()));
    }
    // SAFETY: loading the ORT CUDA provider shared library, exactly as ONNX
    // Runtime itself does when registering the CUDA EP. We drop it immediately;
    // this only checks that its runtime dependencies resolve.
    unsafe { libloading::Library::new(&path) }
        .map(|_lib| ())
        .or_else(|e| {
            let err = e.to_string();
            // ORT provider plugins are normally loaded by libonnxruntime itself.
            // A direct dlopen may fail on ORT host symbols after CUDA/cuDNN deps
            // have resolved; that is still enough for this dependency probe.
            if err.contains("Provider_GetHost") {
                Ok(())
            } else {
                Err(err)
            }
        })
}

/// Cached result of [`probe_cuda_runtime`]; the dlopen runs at most once.
#[cfg(feature = "ort-cuda")]
fn cuda_runtime_available() -> bool {
    static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| probe_cuda_runtime().is_ok())
}

/// User-facing message shown when the CUDA runtime is missing, including the
/// exact commands to install the required libraries.
#[cfg(feature = "ort-cuda")]
fn cuda_missing_message(probe_err: &str) -> String {
    format!(
        "GPU requested (via {env}) but the CUDA runtime libraries required by ONNX Runtime \
         could not be loaded — embedding is running on CPU. Loader error: {probe_err}\n\
         ONNX Runtime 1.{ort_minor}.x needs CUDA 12 + cuDNN 9. Missing libraries typically \
         include: libcudart.so.12, libcublas.so.12, libcublasLt.so.12, libcudnn.so.9, \
         libcurand.so.10, libcufft.so.11.\n\
         Install them on Ubuntu / WSL2:\n  \
         wget -O /tmp/cuda-keyring_1.1-1_all.deb https://developer.download.nvidia.com/compute/cuda/repos/wsl-ubuntu/x86_64/cuda-keyring_1.1-1_all.deb\n  \
         sudo dpkg -i /tmp/cuda-keyring_1.1-1_all.deb && rm -f /tmp/cuda-keyring_1.1-1_all.deb && sudo apt-get update\n  \
         sudo apt-get install -y cuda-cudart-12-8 libcublas-12-8 libcurand-12-8 libcufft-12-8\n  \
         python3 -m venv $HOME/.local/share/lean-ctx/cuda-libs\n  \
         $HOME/.local/share/lean-ctx/cuda-libs/bin/python -m pip install nvidia-cudnn-cu12==9.8.0.87\n  \
         # then ensure the loader can find them (if not already on the path):\n  \
         export LD_LIBRARY_PATH=$($HOME/.local/share/lean-ctx/cuda-libs/bin/python -c 'import pathlib, nvidia.cudnn; print(pathlib.Path(nvidia.cudnn.__file__).parent / '\''lib'\'')'):/usr/local/cuda-12.8/targets/x86_64-linux/lib:/usr/lib/x86_64-linux-gnu:$LD_LIBRARY_PATH\n\
         To silence this and stay on CPU, set {env}=cpu.",
        env = PROVIDER_ENV,
        ort_minor = ort::MINOR_VERSION,
    )
}

pub fn execution_provider_help() -> &'static str {
    "By default lean-ctx auto-detects GPU runtimes from ORT_DYLIB_PATH and otherwise uses CPU. Set LEAN_CTX_ORT_EXECUTION_PROVIDER=cpu|gpu|auto to override."
}

fn cpu_execution_providers() -> Vec<ort::ep::ExecutionProviderDispatch> {
    vec![ort::ep::CPU::default().build()]
}

/// Build the list of GPU execution providers in registration-priority order.
pub fn gpu_execution_providers() -> Vec<ort::ep::ExecutionProviderDispatch> {
    #[allow(unused_mut)]
    let mut eps: Vec<ort::ep::ExecutionProviderDispatch> = Vec::new();
    let compiled_gpu_count = compiled_gpu_provider_names().len();

    #[cfg(feature = "ort-cuda")]
    {
        tracing::info!("Enabling CUDA execution provider for ONNX Runtime");
        eps.push(ort::ep::CUDA::default().build());
    }

    #[cfg(feature = "ort-rocm")]
    {
        tracing::info!("Enabling ROCm execution provider for ONNX Runtime");
        eps.push(ort::ep::ROCm::default().build());
    }

    #[cfg(feature = "ort-webgpu")]
    {
        tracing::info!("Enabling WebGPU execution provider for ONNX Runtime");
        eps.push(ort::ep::WebGPU::default().build());
    }
    #[cfg(all(target_os = "windows", feature = "ort-directml"))]
    {
        tracing::info!("Enabling DirectML execution provider for ONNX Runtime");
        eps.push(ort::ep::DirectML::default().build());
    }

    #[cfg(all(any(target_os = "macos", target_os = "ios"), feature = "ort-coreml"))]
    {
        tracing::info!("Enabling CoreML execution provider for ONNX Runtime");
        eps.push(ort::ep::CoreML::default().build());
    }

    if compiled_gpu_count == 0 {
        tracing::warn!(
            "GPU execution provider requested, but this lean-ctx binary was built without ort-cuda/ort-rocm/etc.; using CPU only"
        );
    } else if eps.is_empty() {
        tracing::debug!("No GPU execution providers configured — using CPU only");
    }

    eps.push(ort::ep::CPU::default().build());
    eps
}

fn provider_policy() -> ProviderPolicy {
    match std::env::var(PROVIDER_ENV) {
        Ok(value) => provider_policy_from_value(&value),
        Err(_) => ProviderPolicy::Auto,
    }
}

fn provider_policy_name() -> &'static str {
    match provider_policy() {
        ProviderPolicy::Cpu => "cpu",
        ProviderPolicy::Gpu => "gpu",
        ProviderPolicy::Auto => "auto",
    }
}

fn provider_policy_from_value(value: &str) -> ProviderPolicy {
    match value.trim().to_lowercase().as_str() {
        "gpu" | "cuda" | "rocm" | "webgpu" | "directml" | "coreml" => ProviderPolicy::Gpu,
        "auto" => ProviderPolicy::Auto,
        _ => ProviderPolicy::Cpu,
    }
}

fn selected_runtime_looks_gpu() -> bool {
    crate::core::ort_environment::resolved_ort_dylib_path()
        .ok()
        .as_deref()
        .is_some_and(runtime_path_looks_gpu)
}

fn runtime_path_looks_gpu(path: &Path) -> bool {
    let path_text = path.to_string_lossy().to_lowercase();
    if path_text.contains("gpu") || path_text.contains("cuda") || path_text.contains("rocm") {
        return true;
    }
    let Some(parent) = path.parent() else {
        return false;
    };
    [
        "libonnxruntime_providers_cuda.so",
        "libonnxruntime_providers_rocm.so",
        "onnxruntime_providers_cuda.dll",
        "onnxruntime_providers_rocm.dll",
        "libonnxruntime_providers_cuda.dylib",
        "libonnxruntime_providers_rocm.dylib",
    ]
    .iter()
    .any(|name| parent.join(name).exists())
}

fn compiled_gpu_provider_names() -> Vec<&'static str> {
    let mut names = vec![
        #[cfg(feature = "ort-cuda")]
        "cuda",
        #[cfg(feature = "ort-rocm")]
        "rocm",
        #[cfg(feature = "ort-webgpu")]
        "webgpu",
        #[cfg(all(target_os = "windows", feature = "ort-directml"))]
        "directml",
        #[cfg(all(any(target_os = "macos", target_os = "ios"), feature = "ort-coreml"))]
        "coreml",
    ];
    let _ = &mut names; // suppress unused_mut when no GPU feature is active
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_policy_defaults_to_cpu_for_unknown_values() {
        assert_eq!(provider_policy_from_value(""), ProviderPolicy::Cpu);
        assert_eq!(provider_policy_from_value("bogus"), ProviderPolicy::Cpu);
        assert_eq!(provider_policy_from_value("cpu"), ProviderPolicy::Cpu);
    }

    #[test]
    fn provider_policy_accepts_gpu_and_auto_aliases() {
        assert_eq!(provider_policy_from_value("gpu"), ProviderPolicy::Gpu);
        assert_eq!(provider_policy_from_value("CUDA"), ProviderPolicy::Gpu);
        assert_eq!(provider_policy_from_value("auto"), ProviderPolicy::Auto);
    }
}

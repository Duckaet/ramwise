//! Provider-based VRAM metrics boundary.
//!
//! GPU memory arrives through vendor tools, not /proc, so every backend
//! hides behind [`VramProvider`]: identity, capability detection, and a
//! sample that is either real data or an explicit failure. The core
//! collector never touches this module, so ramwise works identically with
//! no drivers, no tools, and no GPU. [`MockVramProvider`] pins the contract
//! for tests; [`NvidiaSmiProvider`] proves the seam against real tooling,
//! while AMD and Intel report detected-but-unsupported until their vendor
//! sampling lands.

#![allow(dead_code)]

use std::ffi::OsString;
use std::path::PathBuf;

/// GPU vendor backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VramBackend {
    Nvidia,
    Amd,
    Intel,
}

impl VramBackend {
    pub fn name(self) -> &'static str {
        match self {
            Self::Nvidia => "nvidia",
            Self::Amd => "amd",
            Self::Intel => "intel",
        }
    }

    /// Vendor CLI this backend shells out to, if any.
    pub fn tool_binary(self) -> Option<&'static str> {
        match self {
            Self::Nvidia => Some("nvidia-smi"),
            Self::Amd => Some("rocm-smi"),
            Self::Intel => Some("intel_gpu_top"),
        }
    }
}

/// One VRAM sample. `available` is false exactly when the data is missing,
/// so consumers never mistake a gap for an idle GPU.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VramSample {
    pub backend: VramBackend,
    pub device: String,
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub available: bool,
    pub reason: String,
}

/// Explicit provider failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VramError {
    ToolMissing(&'static str),
    ToolFailed(String),
    Unsupported(String),
}

impl std::fmt::Display for VramError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ToolMissing(binary) => write!(formatter, "{binary} not on PATH"),
            Self::ToolFailed(reason) => write!(formatter, "vendor tool failed: {reason}"),
            Self::Unsupported(reason) => write!(formatter, "unsupported: {reason}"),
        }
    }
}

/// Capability and sampling contract every backend implements.
pub trait VramProvider {
    fn backend(&self) -> VramBackend;
    fn name(&self) -> String;
    /// Detect without sampling: tool presence and driver sanity only.
    fn detect(&self) -> Result<(), VramError>;
    /// Take one sample; failures stay explicit.
    fn sample(&self) -> Result<VramSample, VramError>;
}

/// PATH lookup shared by vendor providers. `path_var` injects fixtures.
pub fn tool_path(binary: &str, path_var: Option<OsString>) -> Option<PathBuf> {
    let path_var = path_var?;
    std::env::split_paths(&path_var)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(binary))
        .find(|candidate| candidate.is_file())
}

/// NVIDIA sampling through `nvidia-smi`. The only backend that shells out
/// today; AMD and Intel reuse the trait with detected-but-unsupported
/// sampling until their parsers land.
pub struct NvidiaSmiProvider {
    path_var: Option<OsString>,
}

impl NvidiaSmiProvider {
    pub fn new(path_var: Option<OsString>) -> Self {
        Self { path_var }
    }

    fn tool(&self) -> Result<PathBuf, VramError> {
        tool_path("nvidia-smi", self.path_var.clone()).ok_or(VramError::ToolMissing("nvidia-smi"))
    }
}

impl VramProvider for NvidiaSmiProvider {
    fn backend(&self) -> VramBackend {
        VramBackend::Nvidia
    }

    fn name(&self) -> String {
        "nvidia-smi".to_string()
    }

    fn detect(&self) -> Result<(), VramError> {
        self.tool().map(|_| ())
    }

    fn sample(&self) -> Result<VramSample, VramError> {
        let tool = self.tool()?;
        let output = std::process::Command::new(&tool)
            .args([
                "--query-gpu=name,memory.total,memory.used",
                "--format=csv,noheader,nounits",
            ])
            .output()
            .map_err(|error| VramError::ToolFailed(error.to_string()))?;
        if !output.status.success() {
            return Err(VramError::ToolFailed(format!(
                "exit {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let (device, total_mib, used_mib) = parse_nvidia_smi_csv(&text)
            .ok_or_else(|| VramError::ToolFailed("unparseable output".into()))?;
        Ok(VramSample {
            backend: VramBackend::Nvidia,
            device,
            total_bytes: total_mib.saturating_mul(1024 * 1024),
            used_bytes: used_mib.saturating_mul(1024 * 1024),
            available: true,
            reason: String::new(),
        })
    }
}

/// Parse one `name, total MiB, used MiB` CSV line. Returns device name and
/// MiB values; anything else is `None`.
pub fn parse_nvidia_smi_csv(text: &str) -> Option<(String, u64, u64)> {
    let line = text.lines().next()?;
    let mut parts: Vec<&str> = line.split(',').map(str::trim).collect();
    if parts.len() < 3 {
        return None;
    }
    let used = parts.pop()?.parse().ok()?;
    let total = parts.pop()?.parse().ok()?;
    let device = parts.join(", ");
    if device.is_empty() {
        return None;
    }
    Some((device, total, used))
}

/// Detected-but-unsupported vendor backend: presence is reported, sampling
/// stays an explicit error until the vendor parser lands.
pub struct UnsupportedVendorProvider {
    backend: VramBackend,
    path_var: Option<OsString>,
}

impl UnsupportedVendorProvider {
    pub fn new(backend: VramBackend, path_var: Option<OsString>) -> Self {
        Self { backend, path_var }
    }
}

impl VramProvider for UnsupportedVendorProvider {
    fn backend(&self) -> VramBackend {
        self.backend
    }

    fn name(&self) -> String {
        self.backend.tool_binary().unwrap_or("unknown").to_string()
    }

    fn detect(&self) -> Result<(), VramError> {
        let binary = self.backend.tool_binary().ok_or_else(|| {
            VramError::Unsupported(format!("{} sampling not implemented", self.backend.name()))
        })?;
        tool_path(binary, self.path_var.clone())
            .map(|_| ())
            .ok_or(VramError::ToolMissing(binary))
    }

    fn sample(&self) -> Result<VramSample, VramError> {
        Err(VramError::Unsupported(format!(
            "{} sampling not yet implemented",
            self.backend.name()
        )))
    }
}

/// Scripted provider pinning the consumer contract in tests.
pub struct MockVramProvider {
    pub backend: VramBackend,
    pub samples: Vec<Result<VramSample, VramError>>,
    pub calls: std::cell::Cell<usize>,
}

impl MockVramProvider {
    pub fn available_sample(backend: VramBackend) -> VramSample {
        VramSample {
            backend,
            device: "Mock GPU".to_string(),
            total_bytes: 8 * 1024 * 1024 * 1024,
            used_bytes: 2 * 1024 * 1024 * 1024,
            available: true,
            reason: String::new(),
        }
    }

    pub fn unavailable_sample(backend: VramBackend, reason: &str) -> VramSample {
        VramSample {
            backend,
            device: String::new(),
            total_bytes: 0,
            used_bytes: 0,
            available: false,
            reason: reason.to_string(),
        }
    }
}

impl VramProvider for MockVramProvider {
    fn backend(&self) -> VramBackend {
        self.backend
    }

    fn name(&self) -> String {
        format!("mock-{}", self.backend.name())
    }

    fn detect(&self) -> Result<(), VramError> {
        Ok(())
    }

    fn sample(&self) -> Result<VramSample, VramError> {
        let call = self.calls.get();
        self.calls.set(call + 1);
        self.samples
            .get(call)
            .cloned()
            .unwrap_or_else(|| Ok(Self::available_sample(self.backend)))
    }
}

/// Detect every backend against one PATH. Vendor implementations stay in
/// this module, far from the core collector.
pub fn detect_providers(path_var: Option<OsString>) -> Vec<Box<dyn VramProvider>> {
    vec![
        Box::new(NvidiaSmiProvider::new(path_var.clone())),
        Box::new(UnsupportedVendorProvider::new(
            VramBackend::Amd,
            path_var.clone(),
        )),
        Box::new(UnsupportedVendorProvider::new(VramBackend::Intel, path_var)),
    ]
}

/// One-line capability report for diagnostics.
pub fn report(path_var: Option<OsString>) -> String {
    detect_providers(path_var)
        .iter()
        .map(|provider| match provider.detect() {
            Ok(()) => format!("{}: tool present", provider.backend().name()),
            Err(error) => format!("{}: {error}", provider.backend().name()),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_path(tools: &[&str]) -> OsString {
        let dir = std::env::temp_dir().join(format!(
            "ramwise-vram-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |duration| duration.as_nanos())
        ));
        std::fs::create_dir_all(&dir).unwrap();
        for tool in tools {
            std::fs::write(dir.join(tool), "#!/bin/sh").unwrap();
        }
        dir.into_os_string()
    }

    #[test]
    fn nvidia_csv_parses_first_device_line() {
        let (device, total, used) =
            parse_nvidia_smi_csv("NVIDIA GeForce RTX 4070, 12282, 1843 \nignored second line\n")
                .unwrap();
        assert_eq!(device, "NVIDIA GeForce RTX 4070");
        assert_eq!(total, 12282);
        assert_eq!(used, 1843);
    }

    #[test]
    fn nvidia_csv_rejects_malformed_output() {
        assert_eq!(parse_nvidia_smi_csv(""), None);
        assert_eq!(parse_nvidia_smi_csv("only, two\n"), None);
        assert_eq!(parse_nvidia_smi_csv("GPU, lots, nope\n"), None);
        assert_eq!(parse_nvidia_smi_csv(", 12282, 1843\n"), None);
    }

    #[test]
    fn missing_tool_is_an_explicit_detection_failure() {
        let provider = NvidiaSmiProvider::new(Some(fixture_path(&[])));
        assert_eq!(provider.detect(), Err(VramError::ToolMissing("nvidia-smi")));
        assert_eq!(provider.sample(), Err(VramError::ToolMissing("nvidia-smi")));
    }

    #[test]
    fn unsupported_vendors_report_presence_but_no_samples() {
        let present =
            UnsupportedVendorProvider::new(VramBackend::Amd, Some(fixture_path(&["rocm-smi"])));
        assert!(present.detect().is_ok());
        assert!(matches!(present.sample(), Err(VramError::Unsupported(_))));
        let absent = UnsupportedVendorProvider::new(VramBackend::Intel, Some(fixture_path(&[])));
        assert_eq!(
            absent.detect(),
            Err(VramError::ToolMissing("intel_gpu_top"))
        );
    }

    #[test]
    fn mock_provider_pins_the_consumer_contract() {
        let provider = MockVramProvider {
            backend: VramBackend::Nvidia,
            samples: vec![
                Ok(MockVramProvider::available_sample(VramBackend::Nvidia)),
                Err(VramError::ToolFailed("driver reset".into())),
            ],
            calls: std::cell::Cell::new(0),
        };
        assert_eq!(provider.backend(), VramBackend::Nvidia);
        assert_eq!(provider.name(), "mock-nvidia");
        assert!(provider.detect().is_ok());
        let first = provider.sample().unwrap();
        assert!(first.available);
        assert_eq!(first.used_bytes, 2 * 1024 * 1024 * 1024);
        // Provider failures stay explicit; consumers must handle Err.
        assert_eq!(
            provider.sample(),
            Err(VramError::ToolFailed("driver reset".into()))
        );
        // Exhausted scripts repeat the default available sample.
        assert!(provider.sample().unwrap().available);
        assert_eq!(provider.calls.get(), 3);
    }

    #[test]
    fn unavailable_samples_carry_reasons_not_zeros() {
        let sample = MockVramProvider::unavailable_sample(VramBackend::Amd, "no driver");
        assert!(!sample.available);
        assert!(!sample.reason.is_empty());
    }

    #[test]
    fn registry_reports_every_backend() {
        let text = report(Some(fixture_path(&[])));
        assert!(text.contains("nvidia:"));
        assert!(text.contains("amd:"));
        assert!(text.contains("intel:"));
    }
}

use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use crate::server_utils::strip_ansi_escape_codes_in_file;

/// Error type for Docker operations
#[derive(Debug, thiserror::Error)]
pub enum DockerError {
    #[error("Docker command failed: {0}")]
    CommandFailed(String),
    #[error("Container not found: {0}")]
    ContainerNotFound(String),
    #[error("Docker not available: {0}")]
    DockerNotAvailable(String),
}

/// Check if a docker image exists locally first, then fall back to a remote
/// registry manifest check. This avoids slow network round-trips when the
/// image has already been pulled.
pub fn docker_image_exists(image: &str) -> bool {
    // Fast local check
    let local = Command::new("docker")
        .args(["image", "inspect", image])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if local {
        return true;
    }
    // Slower remote check
    Command::new("docker")
        .args(["manifest", "inspect", image])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn docker_build_image(
    dockerfile: &Path,
    context_dir: &Path,
    tag: &str,
    build_args: &[(&str, &str)],
) -> Result<(), DockerError> {
    if !docker_available() {
        return Err(DockerError::DockerNotAvailable(
            "Docker is not installed or not in PATH".to_string(),
        ));
    }

    let mut cmd = Command::new("docker");
    cmd.arg("build")
        .arg("--platform")
        .arg("linux/amd64")
        .arg("-f")
        .arg(dockerfile)
        .arg("-t")
        .arg(tag);

    for (k, v) in build_args {
        cmd.arg("--build-arg").arg(format!("{}={}", k, v));
    }

    let status = cmd
        .arg(context_dir)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|e| {
            DockerError::CommandFailed(format!("Failed to execute docker build: {}", e))
        })?;

    if !status.success() {
        return Err(DockerError::CommandFailed(format!(
            "docker build failed with status: {}",
            status
        )));
    }

    Ok(())
}

pub fn docker_pull_image(image: &str) -> Result<(), DockerError> {
    if !docker_available() {
        return Err(DockerError::DockerNotAvailable(
            "Docker is not installed or not in PATH".to_string(),
        ));
    }

    let output = Command::new("docker")
        .arg("pull")
        .args(["--platform", "linux/amd64"])
        .arg(image)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| DockerError::CommandFailed(format!("Failed to execute docker pull: {}", e)))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(DockerError::CommandFailed(format!(
            "docker pull failed with status: {}\n{}",
            output.status, stderr
        )));
    }

    // Print only the "Status:" line (e.g. "Status: Image is up to date for …")
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if line.starts_with("Status:") {
            println!("{}", line);
            break;
        }
    }

    Ok(())
}

pub fn docker_tag_image(source: &str, target: &str) -> Result<(), DockerError> {
    if !docker_available() {
        return Err(DockerError::DockerNotAvailable(
            "Docker is not installed or not in PATH".to_string(),
        ));
    }

    let status = Command::new("docker")
        .args(["tag", source, target])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| DockerError::CommandFailed(format!("Failed to execute docker tag: {}", e)))?;

    if !status.success() {
        return Err(DockerError::CommandFailed(format!(
            "docker tag failed with status: {}",
            status
        )));
    }

    Ok(())
}

pub fn docker_create_container(image: &str) -> Result<String, DockerError> {
    if !docker_available() {
        return Err(DockerError::DockerNotAvailable(
            "Docker is not installed or not in PATH".to_string(),
        ));
    }

    let output = Command::new("docker")
        .args(["create", image])
        .output()
        .map_err(|e| {
            DockerError::CommandFailed(format!("Failed to execute docker create: {}", e))
        })?;

    if !output.status.success() {
        return Err(DockerError::CommandFailed(format!(
            "docker create failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub fn docker_cp_from_container(
    container_id: &str,
    container_src: &str,
    host_dst: &Path,
) -> Result<(), DockerError> {
    if !docker_available() {
        return Err(DockerError::DockerNotAvailable(
            "Docker is not installed or not in PATH".to_string(),
        ));
    }

    let status = Command::new("docker")
        .args(["cp"])
        .arg(format!("{}:{}", container_id, container_src))
        .arg(host_dst)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|e| DockerError::CommandFailed(format!("Failed to execute docker cp: {}", e)))?;

    if !status.success() {
        return Err(DockerError::CommandFailed(format!(
            "docker cp failed with status: {}",
            status
        )));
    }

    Ok(())
}

pub fn docker_rm_container(container_id: &str) -> Result<(), DockerError> {
    if !docker_available() {
        return Err(DockerError::DockerNotAvailable(
            "Docker is not installed or not in PATH".to_string(),
        ));
    }

    let status = Command::new("docker")
        .args(["rm", "-f"])
        .arg(container_id)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| DockerError::CommandFailed(format!("Failed to execute docker rm: {}", e)))?;

    if !status.success() {
        return Err(DockerError::CommandFailed(format!(
            "docker rm failed with status: {}",
            status
        )));
    }

    Ok(())
}

/// Check if Docker is available
pub fn docker_available() -> bool {
    Command::new("docker").arg("--version").output().is_ok()
}

/// Start a Docker container by name
pub fn start_container(container_name: &str) -> Result<Output, DockerError> {
    if !docker_available() {
        return Err(DockerError::DockerNotAvailable(
            "Docker is not installed or not in PATH".to_string(),
        ));
    }

    let output = Command::new("docker")
        .arg("start")
        .arg(container_name)
        .output()
        .map_err(|e| {
            DockerError::CommandFailed(format!("Failed to execute docker start: {}", e))
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("No such container") {
            return Err(DockerError::ContainerNotFound(container_name.to_string()));
        }
        return Err(DockerError::CommandFailed(format!(
            "docker start failed: {}",
            stderr
        )));
    }

    Ok(output)
}

/// Stop a Docker container by name
pub fn stop_container(container_name: &str) -> Result<Output, DockerError> {
    if !docker_available() {
        return Err(DockerError::DockerNotAvailable(
            "Docker is not installed or not in PATH".to_string(),
        ));
    }

    let output = Command::new("docker")
        .arg("stop")
        .arg(container_name)
        .output()
        .map_err(|e| DockerError::CommandFailed(format!("Failed to execute docker stop: {}", e)))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("No such container") {
            return Err(DockerError::ContainerNotFound(container_name.to_string()));
        }
        return Err(DockerError::CommandFailed(format!(
            "docker stop failed: {}",
            stderr
        )));
    }

    Ok(output)
}

/// Run a Docker container (create and start)
pub fn run_container(
    image: &str,
    container_name: &str,
    args: &[&str],
) -> Result<Output, DockerError> {
    if !docker_available() {
        return Err(DockerError::DockerNotAvailable(
            "Docker is not installed or not in PATH".to_string(),
        ));
    }

    let mut cmd = Command::new("docker");
    cmd.arg("run").arg("-d").arg("--name").arg(container_name);

    for arg in args {
        cmd.arg(arg);
    }

    cmd.arg(image);

    let output = cmd
        .output()
        .map_err(|e| DockerError::CommandFailed(format!("Failed to execute docker run: {}", e)))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(DockerError::CommandFailed(format!(
            "docker run failed: {}",
            stderr
        )));
    }

    Ok(output)
}

/// Remove a Docker container
pub fn remove_container(container_name: &str, force: bool) -> Result<Output, DockerError> {
    if !docker_available() {
        return Err(DockerError::DockerNotAvailable(
            "Docker is not installed or not in PATH".to_string(),
        ));
    }

    let mut cmd = Command::new("docker");
    cmd.arg("rm");
    if force {
        cmd.arg("-f");
    }
    cmd.arg(container_name);

    let output = cmd
        .output()
        .map_err(|e| DockerError::CommandFailed(format!("Failed to execute docker rm: {}", e)))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("No such container") {
            return Err(DockerError::ContainerNotFound(container_name.to_string()));
        }
        return Err(DockerError::CommandFailed(format!(
            "docker rm failed: {}",
            stderr
        )));
    }

    Ok(output)
}

/// Check if a container is running
pub fn is_container_running(container_name: &str) -> Result<bool, DockerError> {
    if !docker_available() {
        return Err(DockerError::DockerNotAvailable(
            "Docker is not installed or not in PATH".to_string(),
        ));
    }

    let output = Command::new("docker")
        .arg("ps")
        .arg("--filter")
        .arg(format!("name={}", container_name))
        .arg("--format")
        .arg("{{.Names}}")
        .output()
        .map_err(|e| DockerError::CommandFailed(format!("Failed to execute docker ps: {}", e)))?;

    if !output.status.success() {
        return Err(DockerError::CommandFailed(format!(
            "docker ps failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout.trim() == container_name)
}

/// Wait for a container to be running (with timeout)
pub fn wait_for_container(container_name: &str, timeout: Duration) -> Result<(), DockerError> {
    let start = std::time::Instant::now();
    let check_interval = Duration::from_millis(500);

    while start.elapsed() < timeout {
        if is_container_running(container_name)? {
            return Ok(());
        }
        std::thread::sleep(check_interval);
    }

    Err(DockerError::CommandFailed(format!(
        "Container {} did not start within {:?}",
        container_name, timeout
    )))
}

/// Stop and remove a container (cleanup)
pub fn cleanup_container(container_name: &str) -> Result<(), DockerError> {
    // Try to stop first (ignore error if already stopped)
    let _ = stop_container(container_name);
    // Remove with force
    remove_container(container_name, true)?;
    Ok(())
}

/// A Docker container abstraction that provides methods for managing container lifecycle
#[derive(Debug)]
pub(crate) struct DockerContainer {
    name: String,
    cleaned_up: std::cell::Cell<bool>,
}

impl DockerContainer {
    /// Create a new DockerContainer instance
    pub(crate) fn new(name: String) -> Self {
        Self {
            name,
            cleaned_up: std::cell::Cell::new(false),
        }
    }

    pub(crate) fn is_cleaned_up(&self) -> bool {
        self.cleaned_up.get()
    }

    /// Check if the container is running
    pub(crate) fn is_running(&self) -> Result<bool, DockerError> {
        is_container_running(&self.name)
    }

    /// Stop the container without removing it.
    /// This allows starting the same container later.
    pub(crate) fn stop(&self) -> Result<(), DockerError> {
        if !self.is_running()? {
            return Ok(());
        }
        stop_container(&self.name)?;
        Ok(())
    }

    /// Start an existing container and wait until it is running.
    pub(crate) fn start(&self, timeout: Duration) -> Result<(), DockerError> {
        if self.is_running()? {
            return Ok(());
        }
        start_container(&self.name)?;
        wait_for_container(&self.name, timeout)?;
        Ok(())
    }

    /// Save container logs to a file, stripping ANSI escape codes
    ///
    /// This function:
    /// 1. Verifies the container exists (running or stopped, but not removed)
    /// 2. Saves logs immediately while container still exists
    /// 3. Strips ANSI escape codes
    ///
    /// IMPORTANT: This must be called BEFORE the container is removed.
    pub(crate) fn save_logs(&self, logs_path: &std::path::Path) -> Result<(), DockerError> {
        // First verify container exists (running or stopped, but not removed)
        // This check MUST happen before we try to get logs
        let check_output = Command::new("docker")
            .arg("ps")
            .arg("-a")
            .arg("--filter")
            .arg(format!("name={}", self.name))
            .arg("--format")
            .arg("{{.Names}}")
            .output()
            .map_err(|e| {
                DockerError::CommandFailed(format!("Failed to check if container exists: {}", e))
            })?;

        let stdout = String::from_utf8_lossy(&check_output.stdout);
        if stdout.trim() != self.name {
            // Container doesn't exist, can't save logs
            return Ok(());
        }

        // IMPORTANT: Save logs immediately while container still exists
        // Don't wait before checking - the container must exist NOW
        let output = Command::new("docker")
            .arg("logs")
            .arg(&self.name)
            .output()
            .map_err(|e| {
                DockerError::CommandFailed(format!(
                    "Failed to execute docker logs for container '{}': {}",
                    self.name, e
                ))
            })?;
        let mut combined = Vec::with_capacity(output.stdout.len() + output.stderr.len());
        combined.extend_from_slice(&output.stdout);
        combined.extend_from_slice(&output.stderr);
        std::fs::write(logs_path, combined).map_err(|e| {
            DockerError::CommandFailed(format!(
                "Failed to write docker logs to '{}': {}",
                logs_path.display(),
                e
            ))
        })?;
        strip_ansi_escape_codes_in_file(logs_path).map_err(|e| {
            DockerError::CommandFailed(format!(
                "Failed to strip ANSI escapes from '{}': {}",
                logs_path.display(),
                e
            ))
        })?;

        // Wait after saving logs to ensure file is written
        std::thread::sleep(std::time::Duration::from_millis(100));

        Ok(())
    }

    /// Kill the container (force stop), then save logs, then remove the container.
    pub(crate) fn kill(&self, logs_path: &std::path::Path) -> Result<(), DockerError> {
        self.cleaned_up.set(true);

        if !docker_available() {
            return Err(DockerError::DockerNotAvailable(
                "Docker is not installed or not in PATH".to_string(),
            ));
        }

        let output = Command::new("docker")
            .arg("kill")
            .arg(&self.name)
            .output()
            .map_err(|e| {
                DockerError::CommandFailed(format!(
                    "Failed to execute docker kill for container '{}': {}",
                    self.name, e
                ))
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if !stderr.contains("No such container") && !stderr.contains("is not running") {
                return Err(DockerError::CommandFailed(format!(
                    "docker kill failed for container '{}': {}",
                    self.name, stderr
                )));
            }
        }

        // Save logs AFTER kill (so we capture the final output), and BEFORE removal.
        let _ = self.save_logs(logs_path);

        cleanup_container(&self.name)
    }
}

impl Drop for DockerContainer {
    fn drop(&mut self) {
        // Only save logs and cleanup if not already done explicitly
        if !self.cleaned_up.get() {
            // Attempt to clean up on drop, but ignore errors
            let _ = cleanup_container(&self.name);
        }
    }
}

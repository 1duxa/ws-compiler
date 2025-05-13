use futures_util::{SinkExt, StreamExt};
use log::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs::{self, File},
    io::{self, Write},
    process::{Command, Stdio},
    time::{Duration, SystemTime},
};
use tempfile::tempdir;
use tokio::{net::TcpListener as TokioTcpListener, time};
use tokio_tungstenite::{accept_async, tungstenite::Message};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum Language {
    Rust,
    Go,
    Python,
}
#[rustfmt::skip]
#[derive(Debug, Deserialize)]
struct CompileRequest {
    language: Language,
    code: String,                                       // Main source code content
    additional_files: Option<HashMap<String, String>>,  // Optional additional files
    run: bool,                                          // Whether to run the code after compilation
    args: Option<Vec<String>>,                          // Optional command line arguments for the program
    timeout_seconds: Option<u64>,                       // Maximum execution time in seconds
}
#[rustfmt::skip]
#[derive(Debug, Serialize)]
struct CompileResponse {
    success: bool,              // Whether compilation succeeded
    compile_stdout: String,     // Compilation standard output
    compile_stderr: String,     // Compilation standard error
    run_stdout: Option<String>, // Program output (if run=true and compilation succeeded)
    run_stderr: Option<String>, // Program error output
    compile_time_ms: u64,       // Compilation time in milliseconds
    run_time_ms: Option<u64>,   // Execution time in milliseconds
    timed_out: bool,            // Whether execution timed out
}

const MEMORY_LIMIT: &str = "256m";
const CPU_LIMIT: &str = "0.5";
const NETWORK_MODE: &str = "none";
const PIDS_LIMIT: &str = "50";

async fn handle_compilation(request: CompileRequest) -> io::Result<CompileResponse> {
    info!("Handling compilation request for {:?}", request.language);

    let temp_dir = tempdir()?;
    let dir_path = temp_dir.path();

    let container_id = format!("compiler-{}", Uuid::new_v4());

    match request.language {
        Language::Rust => {
            fs::create_dir_all(dir_path.join("src"))?;

            let main_file = dir_path.join("src/main.rs");
            let mut file = File::create(&main_file)?;
            file.write_all(request.code.as_bytes())?;
            let cargo_toml = r#"
            [package]
            name = "temp_project"
            version = "0.1.0"
            edition = "2021"
            [dependencies]
            "#;
            let mut cargo_file = File::create(dir_path.join("Cargo.toml"))?;
            cargo_file.write_all(cargo_toml.as_bytes())?;
        }
        Language::Go => {
            let main_file = dir_path.join("main.go");
            let mut file = File::create(&main_file)?;
            file.write_all(request.code.as_bytes())?;
            let mut mod_file = File::create(dir_path.join("go.mod"))?;
            mod_file.write_all(b"module tempproject\n\ngo 1.19\n")?;
        }
        Language::Python => {
            let main_file = dir_path.join("main.py");
            let mut file = File::create(&main_file)?;
            file.write_all(request.code.as_bytes())?;
            // Create a requirements.txt file (empty by default)
            let mut requirements_file = File::create(dir_path.join("requirements.txt"))?;
            requirements_file.write_all(b"")?;
        }
    };
    if let Some(files) = &request.additional_files {
        for (path, content) in files {
            let file_path = dir_path.join(path);
            if let Some(parent) = file_path.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut file = File::create(file_path)?;
            file.write_all(content.as_bytes())?;
        }
    }

    let dockerfile_content = match request.language {
        Language::Rust => {
            r#"FROM rust:slim
            WORKDIR /app
            COPY . .
            RUN cargo build --release
            ENTRYPOINT ["./target/release/temp_project"]
            "#
        }
        Language::Go => {
            r#"
            FROM golang:alpine
            ENV GO111MODULE=on
            WORKDIR /app
            COPY . .
            RUN ls -R
            RUN go mod tidy
            RUN go build -o program
            ENTRYPOINT ["./program"]
            "#
        }
        Language::Python => {
            r#"
            FROM python:3.9-slim
            WORKDIR /app
            COPY . .
            RUN pip install --no-cache-dir -r requirements.txt
            ENTRYPOINT ["python", "main.py"]
            "#
        }
    };

    let mut dockerfile = File::create(dir_path.join("Dockerfile"))?;
    dockerfile.write_all(dockerfile_content.as_bytes())?;

    let compile_start = SystemTime::now();
    let build_output = Command::new("docker")
        .arg("build")
        .arg("-t")
        .arg(&container_id)
        .arg(dir_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;

    let compile_success = build_output.status.success();
    let compile_stdout = String::from_utf8_lossy(&build_output.stdout).into_owned();
    let compile_stderr = String::from_utf8_lossy(&build_output.stderr).into_owned();

    let compile_time = SystemTime::now()
        .duration_since(compile_start)
        .unwrap_or_default()
        .as_millis() as u64;

    if !compile_success || !request.run {
        if compile_success {
            let _ = Command::new("docker")
                .args(["rmi", &container_id, "-f"])
                .output();
        }
        return Ok(CompileResponse {
            success: compile_success,
            compile_stdout,
            compile_stderr,
            run_stdout: None,
            run_stderr: None,
            compile_time_ms: compile_time,
            run_time_ms: None,
            timed_out: false,
        });
    }
    let run_start = SystemTime::now();
    let timeout_duration = Duration::from_secs(request.timeout_seconds.unwrap_or(10));
    let mut cmd = Command::new("docker");
    cmd.arg("run")
        .arg("--rm")
        .arg("--memory")
        .arg(MEMORY_LIMIT)
        .arg("--cpus")
        .arg(CPU_LIMIT)
        .arg("--network")
        .arg(NETWORK_MODE)
        .arg("--pids-limit")
        .arg(PIDS_LIMIT)
        .arg("--name")
        .arg(&format!("{}-run", container_id))
        .arg(&container_id);

    if let Some(args) = &request.args {
        cmd.args(args);
    }
    let execution = async move {
        let output = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).output()?;

        let run_time = SystemTime::now()
            .duration_since(run_start)
            .unwrap_or_default()
            .as_millis() as u64;

        let run_stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let run_stderr = String::from_utf8_lossy(&output.stderr).into_owned();

        Result::<_, io::Error>::Ok((run_stdout, run_stderr, run_time))
    };
    let execution_result = time::timeout(timeout_duration, execution).await;
    let _ = Command::new("docker")
        .args(["rmi", &container_id, "-f"])
        .output();

    if execution_result.is_err() {
        let _ = Command::new("docker")
            .args(["stop", &format!("{}-run", container_id)])
            .output();
        let _ = Command::new("docker")
            .args(["rm", &format!("{}-run", container_id), "-f"])
            .output();
    }

    match execution_result {
        Ok(Ok((run_stdout, run_stderr, run_time))) => Ok(CompileResponse {
            success: true,
            compile_stdout,
            compile_stderr,
            run_stdout: Some(run_stdout),
            run_stderr: Some(run_stderr),
            compile_time_ms: compile_time,
            run_time_ms: Some(run_time),
            timed_out: false,
        }),
        Ok(Err(e)) => Ok(CompileResponse {
            success: true,
            compile_stdout,
            compile_stderr,
            run_stdout: None,
            run_stderr: Some(format!("Error executing program: {}", e)),
            compile_time_ms: compile_time,
            run_time_ms: None,
            timed_out: false,
        }),
        Err(_) => Ok(CompileResponse {
            success: true,
            compile_stdout,
            compile_stderr,
            run_stdout: None,
            run_stderr: Some(format!(
                "Execution timed out after {} seconds",
                timeout_duration.as_secs()
            )),
            compile_time_ms: compile_time,
            run_time_ms: Some(timeout_duration.as_millis() as u64),
            timed_out: true,
        }),
    }
}

async fn handle_websocket(stream: tokio::net::TcpStream) {
    info!("New WebSocket connection");

    let ws_stream = match accept_async(stream).await {
        Ok(ws) => ws,
        Err(e) => {
            error!("Error during WebSocket handshake: {:?}", e);
            return;
        }
    };

    let (mut ws_sender, mut ws_receiver) = ws_stream.split();

    while let Some(msg) = ws_receiver.next().await {
        match msg {
            Ok(Message::Text(text)) => {
                debug!("Received request: {}", text);

                match serde_json::from_str::<CompileRequest>(&text) {
                    Ok(request) => {
                        let compilation_result = handle_compilation(request).await;

                        match compilation_result {
                            Ok(response) => {
                                if let Err(e) = ws_sender
                                    .send(Message::Text(serde_json::to_string(&response).unwrap()))
                                    .await
                                {
                                    error!("Failed to send compilation response: {:?}", e);
                                    break;
                                }
                            }
                            Err(e) => {
                                let error_response = CompileResponse {
                                    success: false,
                                    compile_stdout: String::new(),
                                    compile_stderr: format!("Server error: {}", e),
                                    run_stdout: None,
                                    run_stderr: None,
                                    compile_time_ms: 0,
                                    run_time_ms: None,
                                    timed_out: false,
                                };

                                if let Err(e) = ws_sender
                                    .send(Message::Text(
                                        serde_json::to_string(&error_response).unwrap(),
                                    ))
                                    .await
                                {
                                    error!("Failed to send error response: {:?}", e);
                                    break;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        error!("Failed to parse request: {:?}", e);

                        let error_msg = format!(
                            "{{\"success\":false,\"compile_stderr\":\"Failed to parse request: {}\"}}",
                            e
                        );
                        if let Err(e) = ws_sender.send(Message::Text(error_msg)).await {
                            error!("Failed to send parse error response: {:?}", e);
                            break;
                        }
                    }
                }
            }
            Ok(Message::Close(_)) => {
                info!("Client closed connection");
                break;
            }
            Ok(_) => {} // Ignore other message types
            Err(e) => {
                error!("WebSocket error: {:?}", e);
                break;
            }
        }
    }

    info!("WebSocket connection closed");
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_default_env()
        .filter(None, log::LevelFilter::Info)
        .init();

    info!("Starting Containerized Compilation Server on 127.0.0.1:9003");
    info!("Supported languages: Rust, Go, Python");

    match Command::new("docker").arg("--version").output() {
        Ok(_) => info!("Docker is available"),
        Err(e) => {
            error!("Docker is not available: {}", e);
            return Err("Docker is required for containerized compilation".into());
        }
    }

    let addr = "127.0.0.1:9003";
    let listener = TokioTcpListener::bind(addr).await?;

    while let Ok((stream, addr)) = listener.accept().await {
        info!("New connection from {}", addr);
        tokio::spawn(async move {
            handle_websocket(stream).await;
        });
    }

    Ok(())
}

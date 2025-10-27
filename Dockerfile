# ============================================================
# 1. Use an official Rust image for building Rust binaries
# ============================================================
FROM rust:1.89 as builder

# Install Python (for running the Python part later)
RUN apt-get update && apt-get install -y \
    build-essential \
    cmake \
    pkg-config \
    python3 \
    python3-venv \
    python3-pip \
    git \
    && rm -rf /var/lib/apt/lists/*

# ============================================================
# 2. Clone and build PathMap (stable tag)
# ============================================================
WORKDIR /app
RUN git clone https://github.com/Adam-Vandervorst/PathMap.git \
    && cd PathMap \
    && git checkout stable \
    && cargo build --release

# ============================================================
# 3. Clone and build MORK (server branch)
# ============================================================
WORKDIR /app
RUN git clone https://github.com/YeabsiraNigusse/MORK.git \
    && cd MORK \
    && git checkout server \
    # 🩵 Patch the binding so it listens on all interfaces
    && sed -i 's/127\.0\.0\.1/0.0.0.0/g' server/src/main.rs || true \
    && cd server \
    && cargo build --release --bin mork-server


# ============================================================
# 4. Set up Python environment for the MORK project
# ============================================================
WORKDIR /app/MORK
RUN python3 -m venv venv \
    && . venv/bin/activate \
    && pip install --upgrade pip \
    && pip install requests

# ============================================================
# 5. Define runtime environment
# ============================================================
FROM debian:bookworm-slim
# Install runtime dependencies: Python, pip, git
RUN apt-get update && apt-get install -y \
    python3 \
    python3-venv \
    python3-pip \
    git \
    && rm -rf /var/lib/apt/lists/*

# Copy built binaries and Python code from builder
COPY --from=builder /app/PathMap /app/PathMap
COPY --from=builder /app/MORK /app/MORK



# Set working directory
WORKDIR /app/MORK


# Expose any server port if needed (for example 8000)
EXPOSE 8000

# Default command to run the server
CMD ["bash", "-c", "./target/release/mork-server"]


## docker pull yeabsiranigusse/mork-server:latest
## docker run -it --rm -p 8000:8000 mork-server:latest

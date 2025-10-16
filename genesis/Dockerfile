# Following the instruction from https://docs.namada.net/introduction/install/source
# This is used as the genesis container to create the chain and join the validators to the network
FROM docker.io/rust:1.81-slim-bookworm AS builder

# Pinning a commit
ARG GIT_SHA

RUN apt-get update -y && \
    apt-get install -y --no-install-recommends \
    make \
    git-core \
    libssl-dev \
    pkg-config \
    libclang-13-dev \
    clang \
    build-essential \
    protobuf-compiler \
    libprotobuf-dev \
    libudev-dev \
    curl \
    wget \
    python3 \
    python3-pip && \
    rm -rf /var/lib/apt/lists/*
RUN pip3 install toml --break-system-packages

ADD --keep-git-dir=true https://github.com/namada-net/namada.git#$GIT_SHA /namada

# Important!
WORKDIR /namada

RUN make build

RUN cargo install wasm-opt --locked
RUN make build-wasm-scripts

# Preparing the network templates for starting the chain
RUN git clone https://github.com/heliaxdev/namada-network-templates.git /namada-network-templates

# Try to slim down the runner
FROM docker.io/rust:1.81-slim-bookworm

ARG GENESIS_TEMPLATE_VERSION

RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install --no-install-recommends -y \
    ca-certificates \
    curl \
    && apt-get clean && rm -rf /var/lib/apt/lists/*

COPY --from=builder /namada/target/release/namada* /usr/local/bin/
COPY --from=builder /namada/wasm/*.wasm /namada/wasm/
COPY --from=builder /namada/wasm/checksums.json /namada/wasm/
COPY --from=builder /namada-network-templates/devnet/$GENESIS_TEMPLATE_VERSION /network-templates

RUN mkdir -p /root/.masp-params \
    && curl -o /root/.masp-params/masp-spend.params -L https://github.com/anoma/masp-mpc/releases/download/namada-trusted-setup/masp-spend.params?raw=true \
    && curl -o /root/.masp-params/masp-output.params -L https://github.com/anoma/masp-mpc/releases/download/namada-trusted-setup/masp-output.params?raw=true \
    && curl -o /root/.masp-params/masp-convert.params -L https://github.com/anoma/masp-mpc/releases/download/namada-trusted-setup/masp-convert.params?raw=true
ENV NAMADA_MASP_PARAMS_DIR=/root/.masp-params

RUN mkdir -p /container_ready

COPY ./entrypoint.sh /entrypoint.sh

# Important!
WORKDIR /namada

ENTRYPOINT ["/entrypoint.sh"]
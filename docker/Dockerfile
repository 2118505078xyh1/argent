# syntax=docker/dockerfile:1
# argent 的容器镜像：要强隔离时，在这里跑 agent 本体。
# 容器只挂载工作区和 argent 自己的数据目录，不给 host 文件系统
# 任意写入权；凭据（~/.argent）显式挂载，因为 agent 就是凭据持有者。
#
# 构建：docker build -t argent -f docker/Dockerfile .
# 使用：docker run --rm -it \
#         -v "$HOME/.argent:/home/argent/.argent" \
#         -v "$PWD:/work" -w /work \
#         argent
#
# 说明：镜像里没有 chromium / 桌面控制 / 播放器等 host 侧工具；
# MCP stdio 服务端需要在镜像里另装或单独容器提供。

FROM rust:1-bookworm AS builder
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p argent-cli

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl git \
    && rm -rf /var/lib/apt/lists/*
RUN useradd --create-home --uid 1000 argent
COPY --from=builder /src/target/release/argent /usr/local/bin/argent
USER argent
ENV HOME=/home/argent
WORKDIR /work
ENTRYPOINT ["argent"]

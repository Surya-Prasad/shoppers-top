# Shopping Sessions: Rust engine.
# Stage 1 compiles with the network available at build time; stage 2 is a small,
# offline runtime image. No FROM pins a platform, so the scorer builds natively.
FROM rust:1.95-slim-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
# cache dependencies in their own layer
RUN mkdir src && echo 'fn main() {}' > src/main.rs && cargo build --release --locked && rm -rf src
COPY src ./src
# Generic x86-64 code: memchr already picks SIMD at run time, and a portable binary
# cannot hit an illegal instruction on whatever machine builds or runs it.
RUN touch src/main.rs && cargo build --release --locked && strip target/release/shopping-sessions

FROM debian:bookworm-slim
# The scorer runs the container as uid 1000; give that uid a name and a home.
RUN useradd --uid 1000 --no-create-home --home-dir /scratch app
COPY --from=build /src/target/release/shopping-sessions /app/shopping-sessions
WORKDIR /app
USER 1000:1000
# Reads $DATA_DIR, writes $OUTPUT_DIR; logs to stderr only; exit 0 = done.
CMD ["/app/shopping-sessions"]

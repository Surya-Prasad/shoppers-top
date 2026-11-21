# Shopping Sessions: Rust engine.
# Stage 1 compiles with the network available at build time; stage 2 is a small,
# offline runtime image. No FROM pins a platform, so the scorer builds natively.
FROM rust:1.95-slim-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
# RW25: target-cpu settings (x86-64-v3 on x86-64 Linux) apply to the image build too
COPY .cargo ./.cargo
# cache dependencies in their own layer
RUN mkdir src && echo 'fn main() {}' > src/main.rs && cargo build --release --locked && rm -rf src
COPY src ./src
# x86-64 builds target x86-64-v3 (AVX2/BMI2; .cargo/config.toml): supported by the scorer's
# Cascade Lake-or-newer Xeons and by every CI runner. Other architectures build generically.
RUN touch src/main.rs && cargo build --release --locked && strip target/release/shopping-sessions

FROM debian:bookworm-slim
# The scorer runs the container as uid 1000; give that uid a name and a home.
RUN useradd --uid 1000 --no-create-home --home-dir /scratch app
COPY --from=build /src/target/release/shopping-sessions /app/shopping-sessions
WORKDIR /app
USER 1000:1000
# Reads $DATA_DIR, writes $OUTPUT_DIR; logs to stderr only; exit 0 = done.
CMD ["/app/shopping-sessions"]

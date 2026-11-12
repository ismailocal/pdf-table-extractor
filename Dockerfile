FROM rust:1.99.1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src/ ./src/
RUN cargo build --release --locked

FROM debian:bookworm-slim
COPY --from=build /src/target/release/pdf-table-extractor /usr/local/bin/pdf-table-extractor
ENTRYPOINT ["/usr/local/bin/pdf-table-extractor", "--pdf"]

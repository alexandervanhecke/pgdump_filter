FROM rust:1.74 AS builder
 
WORKDIR /app 

COPY . .
 
RUN cargo build --release 

FROM debian:bookworm-slim

COPY --from=builder /app/target/release/pgdump_filter /usr/local/bin/pgdump_filter

ENTRYPOINT ["/usr/local/bin/pgdump_filter"]

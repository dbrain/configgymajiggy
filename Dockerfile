FROM amd64/rust:1.43.1 AS build-stage

WORKDIR /usr/src/biboop
COPY . .

RUN cargo build --release

FROM scratch AS export-stage
COPY --from=build-stage /usr/src/biboop/target/release/biboop ./biboop-amd64

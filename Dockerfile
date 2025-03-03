# main container
FROM docker.io/ubuntu:noble

ENV DEBIAN_FRONTEND=noninteractive
ENV PATH="/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:/root/.cargo/bin/:/root/.dotnet/tools"

RUN apt-get update && \
    apt-get install -y --no-install-recommends curl && \
    apt-get clean

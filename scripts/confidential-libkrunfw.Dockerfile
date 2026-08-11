FROM docker.io/library/debian@sha256:ae614fe11cb373155bf26b938154c34bed87aa701f2f55a4ef03f872e4314ab0

ARG DEBIAN_SNAPSHOT=20251201T000000Z

RUN printf '%s\n' \
      "deb [check-valid-until=no] http://snapshot.debian.org/archive/debian/${DEBIAN_SNAPSHOT}/ trixie main" \
      > /etc/apt/sources.list \
    && rm -f /etc/apt/sources.list.d/debian.sources \
    && apt-get -o Acquire::Check-Valid-Until=false update \
    && DEBIAN_FRONTEND=noninteractive apt-get install --yes --no-install-recommends \
      bc \
      binutils \
      bison \
      build-essential \
      ca-certificates \
      curl \
      flex \
      git \
      libelf-dev \
      libssl-dev \
      python3 \
      python3-pyelftools \
      xz-utils \
    && rm -rf /var/lib/apt/lists/*

ENV LANG=C.UTF-8 \
    LC_ALL=C.UTF-8 \
    TZ=UTC

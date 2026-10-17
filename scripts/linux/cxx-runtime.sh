#!/usr/bin/env bash
# ONNX Runtime uses a newer C++ ABI than Debian 10; this private runtime also
# targets an old glibc. Never install it over the host's libstdc++ or libc.
set -euo pipefail
case "$(uname -m)" in
  x86_64)
    subdir=linux-64
    name=libstdcxx-ng-12.2.0-h46fd767_19.tar.bz2
    digest=0289e6a7b9a5249161a3967909e12dcfb4ab4475cdede984635d3fb65c606f08
    ;;
  aarch64)
    subdir=linux-aarch64
    name=libstdcxx-ng-12.2.0-hc13a102_19.tar.bz2
    digest=db906f0ad19acc6aefcd5409a7a72fea76302f72013dce7593467ae07dbf54f3
    ;;
  *) echo "Unsupported C++ runtime architecture" >&2; exit 1 ;;
esac
mkdir -p /opt/compat/source
if [[ -f "/opt/compat/downloads/$name" ]]; then
  cp "/opt/compat/downloads/$name" "/opt/compat/source/$name"
else
  curl -fL --retry 4 "https://conda.anaconda.org/conda-forge/$subdir/$name" -o "/opt/compat/source/$name"
fi
echo "$digest  /opt/compat/source/$name" | sha256sum --check
tar -xjf "/opt/compat/source/$name" -C /opt/compat

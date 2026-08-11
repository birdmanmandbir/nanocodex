package policy

import future.keywords.every

default nv_match := false

nv_match {
    count(input) == 1
    every result in input {
        result["x-nvidia-device-type"] == "gpu"
        result.hwmodel == "GH100 A01 GSP BROM"
        result.measres == "success"
        result.secboot
        result.dbgstat == "disabled"
        result["x-nvidia-gpu-arch-check"]
        result["x-nvidia-gpu-attestation-report-cert-chain-fwid-match"]
        result["x-nvidia-gpu-attestation-report-nonce-match"]
        result["x-nvidia-gpu-attestation-report-parsed"]
        result["x-nvidia-gpu-attestation-report-signature-verified"]
        result["x-nvidia-gpu-driver-rim-fetched"]
        result["x-nvidia-gpu-driver-rim-measurements-available"]
        result["x-nvidia-gpu-driver-rim-signature-verified"]
        result["x-nvidia-gpu-driver-rim-version-match"]
        result["x-nvidia-gpu-vbios-rim-fetched"]
        result["x-nvidia-gpu-vbios-rim-measurements-available"]
        result["x-nvidia-gpu-vbios-rim-signature-verified"]
        result["x-nvidia-gpu-vbios-rim-version-match"]
        result["x-nvidia-mismatch-measurement-records"] == null
    }
}

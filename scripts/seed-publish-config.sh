#!/usr/bin/env bash
# Writes the microscope.toml a published image is built from, and the image tag,
# from the comma-separated PROGRAM_ID and IDL_PATH lists.
set -euo pipefail

IFS=, read -ra program_ids <<<"${PROGRAM_ID// /}"
IFS=, read -ra idl_paths <<<"${IDL_PATH// /}"

if ((${#program_ids[@]} == 0 || ${#program_ids[@]} != ${#idl_paths[@]})); then
    echo "::error::program_id and idl_path must list the same number of entries" >&2
    exit 1
fi

for index in "${!program_ids[@]}"; do
    if [[ ! "${program_ids[$index]}" =~ ^[1-9A-HJ-NP-Za-km-z]{32,44}$ ]]; then
        echo "::error::${program_ids[$index]} is not a base58 program ID" >&2
        exit 1
    fi
    if [[ ! -f "${idl_paths[$index]}" ]]; then
        echo "::error::${idl_paths[$index]} is not in the repository" >&2
        exit 1
    fi
    if [[ "${idl_paths[$index]}" != idl/* ]]; then
        echo "::error::idl_path must be under idl/, which is the only IDL directory the image contains" >&2
        exit 1
    fi
done

if ((${#program_ids[@]} == 1)); then
    printf 'program_id = "%s"\nidl_path = "%s"\n' "${program_ids[0]}" "${idl_paths[0]}" >microscope.toml
    tag="${program_ids[0]}"
else
    for index in "${!program_ids[@]}"; do
        printf '[[programs]]\nprogram_id = "%s"\nidl_path = "%s"\n\n' \
            "${program_ids[$index]}" "${idl_paths[$index]}"
    done >microscope.toml
    # Every program ID would overflow the 128-character tag limit, so the tag
    # names the first and fingerprints the ordered list.
    list="$(IFS=,; printf '%s' "${program_ids[*]}")"
    tag="${program_ids[0]}-plus-$(printf '%s' "$list" | sha256sum | cut -c1-8)"
fi

echo "tag=$tag" >>"${GITHUB_OUTPUT:-/dev/stdout}"

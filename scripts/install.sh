#!/bin/sh
set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)

if ! command -v cargo >/dev/null 2>&1 || ! command -v rustc >/dev/null 2>&1; then
    printf '%s\n' "Rust is required to build Venus Agent."
    printf '%s' "Install the minimal Rust toolchain from the official Rustup site now? [y/N] "
    read -r answer
    case "$answer" in
        y|Y|yes|YES)
            ;;
        *)
            printf '%s\n' "Install Rust from https://rustup.rs, then rerun this script."
            exit 1
            ;;
    esac

    if ! command -v curl >/dev/null 2>&1; then
        printf '%s\n' "curl is required to download Rustup. Install Rust from https://rustup.rs, then rerun this script." >&2
        exit 1
    fi
    installer=$(mktemp)
    if ! curl --fail --location --proto '=https' --tlsv1.2 --silent --show-error \
        https://sh.rustup.rs -o "$installer"; then
        rm -f "$installer"
        printf '%s\n' "Could not download the official Rustup installer." >&2
        exit 1
    fi
    if ! sh "$installer" -y --profile minimal; then
        rm -f "$installer"
        printf '%s\n' "Rustup installation failed." >&2
        exit 1
    fi
    rm -f "$installer"
    PATH="$HOME/.cargo/bin:$PATH"
    export PATH
fi

if ! command -v cc >/dev/null 2>&1 && ! command -v clang >/dev/null 2>&1; then
    printf '%s\n' "A C compiler is required by a bundled Rust dependency." >&2
    printf '%s\n' "Install Xcode Command Line Tools on macOS, or your distribution's build tools, then rerun this script." >&2
    exit 1
fi

cargo install --path "$repo_root" --locked --force

cargo_bin="$HOME/.cargo/bin"
case ":$PATH:" in
    *":$cargo_bin:"*)
        ;;
    *)
        shell_name=${SHELL:-}
        shell_name=${shell_name##*/}
        shell_config=
        case "$shell_name" in
            zsh)
                shell_config="$HOME/.zshrc"
                ;;
            bash)
                if [ "$(uname -s)" = "Darwin" ]; then
                    shell_config="$HOME/.bash_profile"
                else
                    shell_config="$HOME/.bashrc"
                fi
                ;;
        esac

        if [ -n "$shell_config" ]; then
            printf '\nCargo installs commands in %s, which is not on PATH.\n' "$cargo_bin"
            printf 'Add it to %s so `venus` and `venus-agent` work in new terminals? [Y/n] ' "$shell_config"
            if IFS= read -r answer && case "$answer" in n|N|no|NO) false ;; *) true ;; esac; then
                path_line='export PATH="$HOME/.cargo/bin:$PATH"'
                if [ -f "$shell_config" ] && grep -F "$path_line" "$shell_config" >/dev/null 2>&1; then
                    printf '%s\n' "The Cargo PATH entry is already configured in $shell_config."
                else
                    if ! printf '\n%s\n' "$path_line" >> "$shell_config"; then
                        printf '%s\n' "Could not update $shell_config. Add this line manually:" >&2
                        printf '%s\n' "$path_line" >&2
                    else
                        printf 'Updated %s.\n' "$shell_config"
                    fi
                fi
                printf 'Run `source %s` now, then you can use `venus` or `venus-agent` in this terminal.\n' "$shell_config"
            else
                printf 'To use the commands in this terminal, run:\nexport PATH="%s:$PATH"\n' "$cargo_bin"
            fi
        else
            printf 'Cargo''s binary directory is not on PATH. Add %s to PATH before running Venus.\n' "$cargo_bin"
            printf 'For this terminal, run: export PATH="%s:$PATH"\n' "$cargo_bin"
        fi
        ;;
esac

printf '%s\n' "Venus Agent is installed. Run 'venus-agent' to start guided setup."

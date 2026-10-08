#!/usr/bin/env fish
# Checks everything fisch-macro needs on CachyOS / Arch + Hyprland.
# Usage: fish scripts/check-deps.fish

set -g problems 0

function ok
    set_color green; echo -n "  ok    "; set_color normal; echo $argv
end

function bad
    set_color red; echo -n "  FIX   "; set_color normal; echo $argv
    set -g problems (math $problems + 1)
end

function note
    set_color yellow; echo -n "  note  "; set_color normal; echo $argv
end

echo "Rust toolchain"
if command -q cargo; and command -q rustc
    set -l ver (rustc --version | string match -r '\d+\.\d+\.\d+')
    set -l parts (string split . $ver)
    if test $parts[1] -gt 1; or test $parts[2] -ge 88
        ok "rustc $ver, "(cargo --version)
    else
        bad "rustc $ver is too old (need 1.88+): rustup update stable"
    end
else
    bad "Rust is missing: sudo pacman -S --needed rustup; and rustup default stable"
end

echo "Hyprland"
if command -q hyprctl
    ok (hyprctl version | head -n 1)
    if set -q HYPRLAND_INSTANCE_SIGNATURE
        ok "running inside a Hyprland session"
        if hyprctl -j status 2>/dev/null | string match -q -r '"configProvider"\s*:\s*"lua"'
            note "config provider: Lua (hyprland.lua) - use the Lua bind lines from the README"
        else
            note "config provider: hyprland.conf - use the hyprland.conf bind lines from the README"
        end
        for line in (hyprctl monitors | string match -r '^Monitor .*|^\s+\d+x\d+@.*|^\s+scale: .*')
            note (string trim $line)
        end
    else
        bad "HYPRLAND_INSTANCE_SIGNATURE is not set - run this from a terminal inside Hyprland"
    end
else
    bad "hyprctl not found: sudo pacman -S hyprland"
end

echo "Screen capture"
if command -q grim
    ok "grim "(command -s grim)" (fallback backend; screencopy is built in)"
else
    note "grim not installed (optional fallback): sudo pacman -S grim"
end

echo "GPU"
if test -r /proc/driver/nvidia/version
    ok (head -n 1 /proc/driver/nvidia/version | string replace -r '.*Kernel Module\s+' 'NVIDIA driver ' | string trim)
    if test -r /sys/module/nvidia_drm/parameters/modeset
        if test (cat /sys/module/nvidia_drm/parameters/modeset) = Y
            ok "nvidia_drm modeset=1"
        else
            bad "nvidia_drm modeset is off (Hyprland needs nvidia_drm.modeset=1)"
        end
    end
else
    note "no NVIDIA kernel module detected"
end

echo "Sober"
if command -q flatpak
    if flatpak info org.vinegarhq.Sober >/dev/null 2>&1
        ok "Sober installed: "(flatpak info org.vinegarhq.Sober | string match -r 'Version:\s*\S+' | string replace -r 'Version:\s*' '')
    else
        bad "Sober is not installed: flatpak install flathub org.vinegarhq.Sober"
    end
else
    bad "flatpak missing: sudo pacman -S flatpak"
end
if command -q hyprctl; and set -q HYPRLAND_INSTANCE_SIGNATURE
    set -l classes (hyprctl -j clients | string match -r -a -g '"class"\s*:\s*"([^"]*)"' | string match -i '*sober*')
    if test (count $classes) -gt 0
        ok "Sober window class: $classes[1] (config.toml [window] class)"
    else
        note "Sober is not running - start it, then check: hyprctl clients | grep class"
    end
end

echo "uinput (virtual mouse/keyboard)"
if test -e /dev/uinput
    if test -w /dev/uinput
        ok "/dev/uinput is writable: "(ls -l /dev/uinput)
    else
        bad "/dev/uinput not writable: run  fish scripts/setup-uinput.fish  then log out and back in"
    end
else
    bad "/dev/uinput missing: run  fish scripts/setup-uinput.fish"
end
if contains input (id -nG | string split ' ')
    ok "user $USER is in the input group"
else
    bad "user $USER is not in the input group (setup-uinput.fish adds it; log out/in afterwards)"
end

echo
if test $problems -eq 0
    set_color green; echo "Everything looks good."; set_color normal
else
    set_color red; echo "$problems thing(s) to fix (see FIX lines above)."; set_color normal
end

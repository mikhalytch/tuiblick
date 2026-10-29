# tuiblick — Linux verification (AmneziaWG only)

OpenVPN is not checked on Linux so far — no OpenVPN steps below.

Run with your tunnel **deactivated**, then paste back the `/tmp/tuiblick-verify/`
files (or their contents) for inspection. Every command below only *reads*
state — except the optional tunnel section at the bottom, which is marked.

Copy-paste the whole block:

```sh
mkdir -p /tmp/tuiblick-verify
cd /run/media/unsafe/code/tuiblick

uname -a > /tmp/tuiblick-verify/uname.txt 2>&1
cat /etc/os-release > /tmp/tuiblick-verify/os-release.txt 2>&1

for bin in awg-quick awg openvpn curl dig ip route sudo resolvconf; do
  command -v "$bin" >> /tmp/tuiblick-verify/which.txt 2>&1
done

awg --version > /tmp/tuiblick-verify/awg-version.txt 2>&1
openvpn --version > /tmp/tuiblick-verify/openvpn-version.txt 2>&1
curl --version > /tmp/tuiblick-verify/curl-version.txt 2>&1
dig -v > /tmp/tuiblick-verify/dig-version.txt 2>&1

# Route probes (the shapes net.rs must parse)
ip route show default > /tmp/tuiblick-verify/ip-route-show.txt 2>&1
ip route get 1.1.1.1 > /tmp/tuiblick-verify/ip-route-get.txt 2>&1
route -n > /tmp/tuiblick-verify/route-n.txt 2>&1
route -n get default > /tmp/tuiblick-verify/route-n-get.txt 2>&1; echo "exit=$?" > /tmp/tuiblick-verify/route-n-get-exit.txt 2>&1

# Exit-IP probes
curl -s --max-time 5 https://api.ipify.org > /tmp/tuiblick-verify/ipify.txt 2>&1; echo "exit=$?" >> /tmp/tuiblick-verify/ipify.txt 2>&1
dig +short +time=3 +tries=1 myip.opendns.com @208.67.222.222 > /tmp/tuiblick-verify/opendns.txt 2>&1; echo "exit=$?" >> /tmp/tuiblick-verify/opendns.txt 2>&1

# Backend state without a tunnel (expect: empty output, exit 0)
awg show > /tmp/tuiblick-verify/awg-show.txt 2>&1; echo "exit=$?" >> /tmp/tuiblick-verify/awg-show.txt 2>&1

# Sudo timestamp state (expect "a password is required", exit 1 when cold)
sudo -n true > /tmp/tuiblick-verify/sudo-n-true.txt 2>&1; echo "exit=$?" >> /tmp/tuiblick-verify/sudo-n-true.txt 2>&1

# Build + full unit gate
cargo test > /tmp/tuiblick-verify/cargo-test.txt 2>&1; echo "exit=$?" >> /tmp/tuiblick-verify/cargo-test.txt 2>&1
./target/debug/tuiblick --help > /tmp/tuiblick-verify/tuiblick-help.txt 2>&1

ls -l /tmp/tuiblick-verify/
```

## Expected shapes (tunnel down)

- `ip-route-show.txt`: `default via <gw> dev <iface> ...` (parser needs `via` + `dev`)
- `ip-route-get.txt`: `<ip> via <gw> dev <iface> src ...` (parser needs `via` + `dev`)
- `route-n-get.txt`: net-tools usage error (confirms the macOS `route -n get`
  syntax does not exist on Linux — the app must never rely on it)
- `ipify.txt` / `opendns.txt`: same public IP, each with `exit=0`
- `awg-show.txt`: empty body with `exit=0`
- `cargo-test.txt`: ends with `test result: ok`

## Optional: live tunnel round-trip (sudo, changes routes — skip if unsure)

Only with a spare test profile. Deactivate everything else first, then:

```sh
sudo awg-quick up /path/to/test-awg0.conf > /tmp/tuiblick-verify/awg-up.txt 2>&1; echo "exit=$?" >> /tmp/tuiblick-verify/awg-up.txt 2>&1
awg show > /tmp/tuiblick-verify/awg-show-up.txt 2>&1
ip route show default > /tmp/tuiblick-verify/ip-route-show-up.txt 2>&1
sudo awg-quick down /path/to/test-awg0.conf > /tmp/tuiblick-verify/awg-down.txt 2>&1; echo "exit=$?" >> /tmp/tuiblick-verify/awg-down.txt 2>&1
```

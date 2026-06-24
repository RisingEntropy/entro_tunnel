#!/usr/bin/env bash
# EntroTunnel — Aliyun server install (WebSocket behind nginx + Let's Encrypt).
#
#   Run as:  sudo bash server2-install.sh
#
# Binary + config live under the invoking user's ~/entrotunnel (per request);
# the systemd service runs them as root. nginx terminates TLS for the domain and
# reverse-proxies BOTH the tunnel and the web admin panel.
#
# Expects these files in the SAME directory as this script:
#   entrotunnel-server                 (the Linux binary)
#   server.toml                        (server config, contains secrets)
#   nginx-tun1.hydeng.cn.conf          (nginx vhost: tunnel + /panel admin)
set -euo pipefail
[ "$(id -u)" -eq 0 ] || { echo "please run with sudo / as root"; exit 1; }
DIR="$(cd "$(dirname "$0")" && pwd)"
DOMAIN=tun1.hydeng.cn
# Home dir of the user who invoked sudo (falls back to /root).
RUNUSER="${SUDO_USER:-root}"
HOME_DIR="$(eval echo "~$RUNUSER")"
APP="$HOME_DIR/entrotunnel"

echo "==> install binary + config into $APP"
install -d -o "$RUNUSER" -g "$RUNUSER" -m 755 "$APP"
install -m755 "$DIR/entrotunnel-server" "$APP/entrotunnel-server"
[ -f "$APP/server.toml" ] || install -m600 "$DIR/server.toml" "$APP/server.toml"
chown -R "$RUNUSER:$RUNUSER" "$APP"

echo "==> systemd unit (runs from $APP)"
cat > /etc/systemd/system/entrotunnel.service <<UNIT
[Unit]
Description=EntroTunnel server
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=$APP/entrotunnel-server -c $APP/server.toml run
WorkingDirectory=$APP
Restart=on-failure
RestartSec=3
Environment=RUST_LOG=info
NoNewPrivileges=false

[Install]
WantedBy=multi-user.target
UNIT

echo "==> install nginx vhost (tunnel + /panel admin)"
install -m644 "$DIR/nginx-$DOMAIN.conf" "/etc/nginx/conf.d/$DOMAIN.conf"
nginx -t
systemctl reload nginx

echo "==> obtain TLS certificate (Let's Encrypt, HTTP-01 via nginx)"
if [ ! -d "/etc/letsencrypt/live/$DOMAIN" ]; then
  certbot --nginx -d "$DOMAIN" --redirect --non-interactive --agree-tos \
    --register-unsafely-without-email \
    || echo "!! certbot failed — verify $DOMAIN resolves here and 80/443 are open"
fi
nginx -t && systemctl reload nginx

echo "==> enable + start service"
systemctl daemon-reload
systemctl enable --now entrotunnel
sleep 2
systemctl --no-pager --full status entrotunnel | head -12

echo
echo "==> done."
echo "    app dir  : $APP"
echo "    tunnel   : wss://$DOMAIN/   (clients: transport=ws, host=$DOMAIN, port=443)"
echo "    admin    : https://$DOMAIN/panel/   (token = web.admin_token in server.toml)"

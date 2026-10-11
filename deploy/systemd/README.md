# systemd

`ferryman.service` runs ferryman as `Type=notify` under a `DynamicUser`.

```sh
sudo install -m755 ferryman /usr/local/bin/ferryman
sudo install -d /etc/ferryman && sudo cp config.toml /etc/ferryman/
sudo cp deploy/systemd/ferryman.service /etc/systemd/system/
sudo systemctl daemon-reload && sudo systemctl enable --now ferryman
```

- Reload: `systemctl reload ferryman` sends SIGHUP (config and TLS re-read).
- TLS: certs are passed with `LoadCredential=` and appear under `%d`
  (`$CREDENTIALS_DIRECTORY`); remove those lines for plain HTTP.
- Logs: JSON on stdout, `journalctl -u ferryman -o cat | jq .`.
- `TimeoutStopSec` must exceed `shutdown_delay_secs` + drain time.

See the systemd quickstart in the [book](https://bunty9.github.io/ferryman/quickstarts.html#systemd).

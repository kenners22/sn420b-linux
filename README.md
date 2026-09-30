# sn420b-linux

Get an **SN 420B / Xprinter XP-420B** 4x6 thermal label printer (sold under several brands, e.g. Vretti) onto **Wi-Fi and printing from Linux**, even when the phone app refuses to connect.

One bash script, no vendor driver. It uses CUPS's built-in Zebra ZPL driver.

## Symptoms this fixes

- The phone app ("Wireless Link" or similar) says **"failed to configure wifi"**, even after holding FEED until the light blinks blue.
- The self-test page shows **`WIFI FUNC: OFF`** and **`WIFI STA IP: 0.0.0.0`**.
- The printer never shows up on your network.
- On Linux, prints fail with **"No pages were found"** and the CUPS log shows `Error setting cupsCompression` / `rangecheck in .putdeviceprops`.
- You put the printer on a **guest** network to "make it work" and now nothing can reach it.

## What's actually going on

1. **The Wi-Fi can be configured over USB, with no app needed.** The printer accepts plain TSPL commands on its USB port. The vendor's own desktop tool uses exactly these:
   ```
   WIFI SCAN                          list networks the printer can see
   WIFI CONNECTION "ssid","password"  set the network
   WIFI RESET                         apply / reconnect
   WIFI GETIP                         reply: +OK=<ip>
   WIFI GETSTATUS                     reply: +OK_..._STATUS
   OUT "marker"                       echo a marker so you know the reply has ended
   ```
   Send them to `/dev/usb/lp0` with `\r\n` line endings and read the reply from the same device.
2. **"2.4 GHz only" isn't the whole story.** The Wi-Fi module on these (Feasycom FSC-BW236) also listed 5 GHz networks in its `WIFI SCAN` during testing, and joined a dual-band router's network fine. If it won't join, check the password and WPA3-only settings before blaming the band.
3. **Guest networks isolate clients.** Put the printer on your main network, or your computers won't be able to reach it.
4. **CUPS Zebra ZPL driver + Ghostscript 10 bug.** The driver's default `Darkness=-1` makes Ghostscript abort, so no pages are ever produced. Setting any real darkness (this script uses 15) fixes it.

## Install

```bash
git clone https://github.com/kenners22/sn420b-linux
cd sn420b-linux
install -m755 sn420b ~/.local/bin/
# optional: permanent USB access for your desktop user
sudo cp 70-sn420b.rules /etc/udev/rules.d/ && sudo udevadm control --reload && sudo udevadm trigger
```
Without the udev rule, grant access for this session with `sudo setfacl -m u:$USER:rw /dev/usb/lp0`.

Needs `bash`, `python3`, `socat`, `cups` and `ping`.

## Use

```bash
sn420b selftest        # (USB) print the config page: check WIFI FUNC / STA IP
sn420b wifi            # (USB) enter SSID + password; prints the IP it gets
sn420b setup <IP>      # create CUPS queue "SN420B" (4x6, ZPL, Darkness 15)
sn420b test            # print a test label over Wi-Fi
lp -d SN420B label.pdf
```

Unplug USB once `sn420b wifi` reports an IP. Then **reserve that IP in your router's DHCP settings** so it never moves. If it does move, `sn420b fix` finds it again and repoints the queue.

| Command | What it does |
|---|---|
| `wifi` | Join Wi-Fi over USB. The password is typed at a hidden prompt, sent only to the printer, never stored |
| `usbip` | Ask the printer over USB for its current Wi-Fi IP |
| `selftest` | Print the self-test / configuration page |
| `find` | Scan your /24 for hosts with port 9100 open |
| `setup [IP]` | Create or update the CUPS queue (uses sudo) |
| `fix` | Printer unreachable? Rescan and repoint the queue |
| `status` | Show the saved IP, whether it's reachable, and the queue state |
| `test [IP]` | Send a ZPL test label straight to port 9100 |

## Tested with

- SN 420B, firmware 1.037, USB `2d37:62de`, Wi-Fi module FSC-BW236 V6.0.2
- Arch-based Linux, CUPS 2.4.19, cups-filters 2.0.1, Ghostscript 10.07

Other Xprinter-family models (the vendor tool also lists the D463B, D465B, 410B, DB402 and DB403) probably accept the same `WIFI` commands. PRs with test reports are welcome.

## Notes

- CUPS prints *"Printer drivers are deprecated"* when adding the queue. That's a warning only. It still works on CUPS 2.x.
- The printer doesn't answer status queries over the network, so `status` only checks whether it's reachable.

## License

MIT

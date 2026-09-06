# PROTOTYPE (throwaway). Real-time headless screenshots of overlay variants via Chrome DevTools.
# usage: uv run --with websockets shoot.py v:scenario:phase+ms[:backdrop] ...
#   e.g.  shoot.py b:long:interim+4000 b:short:finalizing+60 b:llm:answering+1200:terminal
#   waits until OverlayProto.state.phase == phase, then ms more, then captures.
# writes /tmp/overlay-proto-shots/<v>-<scenario>-<phase>+<ms>-<backdrop>.png and prints the paths
import asyncio
import base64
import json
import os
import subprocess
import sys
import time
import urllib.request

import websockets

PORT = 9333
OUT = "/tmp/overlay-proto-shots"


def ensure_chrome():
    try:
        urllib.request.urlopen(f"http://localhost:{PORT}/json/version", timeout=1)
        return
    except OSError:
        pass
    subprocess.Popen(
        [
            "google-chrome",
            "--headless=new",
            "--disable-gpu",
            "--hide-scrollbars",
            "--window-size=1920,1080",
            f"--remote-debugging-port={PORT}",
            "--user-data-dir=/tmp/overlay-proto-chrome",
            "about:blank",
        ],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    for _ in range(50):
        try:
            urllib.request.urlopen(f"http://localhost:{PORT}/json/version", timeout=1)
            return
        except OSError:
            time.sleep(0.2)
    raise SystemExit("chrome did not start")


async def main(specs):
    ensure_chrome()
    pages = json.load(urllib.request.urlopen(f"http://localhost:{PORT}/json"))
    page = next(p for p in pages if p["type"] == "page")
    os.makedirs(OUT, exist_ok=True)
    async with websockets.connect(page["webSocketDebuggerUrl"], max_size=64 * 1024 * 1024) as ws:
        mid = 0

        async def call(method, **params):
            nonlocal mid
            mid += 1
            await ws.send(json.dumps({"id": mid, "method": method, "params": params}))
            while True:
                msg = json.loads(await ws.recv())
                if msg.get("id") == mid:
                    return msg.get("result", {})

        await call(
            "Emulation.setDeviceMetricsOverride",
            width=1920,
            height=1080,
            deviceScaleFactor=1,
            mobile=False,
        )
        await call("Network.enable")
        await call("Network.setCacheDisabled", cacheDisabled=True)
        for spec in specs:
            parts = spec.split(":")
            v, scn, when = parts[0], parts[1], parts[2]
            bd = parts[3] if len(parts) > 3 else "chat"
            phase, _, extra = when.partition("+")
            extra_ms = int(extra or 0)
            v, _, q = v.partition("?")
            url = f"http://localhost:8765/?variant={v}&backdrop={bd}&auto={scn}"
            if q:
                url += "&" + q
            tag = v + ("-" + q.split("=")[-1] if q else "")
            await call("Page.navigate", url=url)
            deadline = time.time() + 40
            while time.time() < deadline:
                r = await call(
                    "Runtime.evaluate",
                    expression="window.OverlayProto ? OverlayProto.state.phase : ''",
                    returnByValue=True,
                )
                if r.get("result", {}).get("value") == phase:
                    break
                await asyncio.sleep(0.03)
            else:
                print(f"timeout waiting for {phase} in {spec}", file=sys.stderr)
                continue
            await asyncio.sleep(extra_ms / 1000)
            shot = await call("Page.captureScreenshot", format="png")
            path = f"{OUT}/{tag}-{scn}-{phase}+{extra_ms}-{bd}.png"
            with open(path, "wb") as f:
                f.write(base64.b64decode(shot["data"]))
            print(path)


asyncio.run(main(sys.argv[1:]))

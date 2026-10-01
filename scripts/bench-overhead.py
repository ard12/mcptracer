#!/usr/bin/env python3
"""Bounded, revision-bound MCPTracer stdio/HTTP resource benchmark.

The fake server is local and synthetic. Results describe this machine/build,
not production reliability or shared-runner latency guarantees.
"""
import argparse
import ctypes
import hashlib
import http.client
import json
import os
import platform
import socket
import statistics
import sqlite3
import subprocess
import sys
import tempfile
import threading
from concurrent.futures import ThreadPoolExecutor
import time
from pathlib import Path


def frame(message):
    return json.dumps(message, separators=(",", ":")).encode() + b"\n"


def percentile(values, p):
    ordered = sorted(values)
    return ordered[min(len(ordered)-1, int(round((len(ordered)-1)*p)))]


def memory_reader(pid):
    if sys.platform == "win32":
        class Counters(ctypes.Structure):
            _fields_ = [("cb", ctypes.c_ulong), ("PageFaultCount", ctypes.c_ulong),
                        ("PeakWorkingSetSize", ctypes.c_size_t), ("WorkingSetSize", ctypes.c_size_t),
                        ("QuotaPeakPagedPoolUsage", ctypes.c_size_t), ("QuotaPagedPoolUsage", ctypes.c_size_t),
                        ("QuotaPeakNonPagedPoolUsage", ctypes.c_size_t), ("QuotaNonPagedPoolUsage", ctypes.c_size_t),
                        ("PagefileUsage", ctypes.c_size_t), ("PeakPagefileUsage", ctypes.c_size_t)]
        process = ctypes.windll.kernel32.OpenProcess(0x0400, False, pid)
        if not process:
            return None, "OpenProcess denied"
        counters = Counters(); counters.cb = ctypes.sizeof(counters)
        ok = ctypes.windll.psapi.GetProcessMemoryInfo(process, ctypes.byref(counters), counters.cb)
        ctypes.windll.kernel32.CloseHandle(process)
        return (int(counters.WorkingSetSize), None) if ok else (None, "GetProcessMemoryInfo failed")
    status = Path(f"/proc/{pid}/status")
    if status.exists():
        for line in status.read_text(errors="replace").splitlines():
            if line.startswith("VmHWM:"):
                return int(line.split()[1]) * 1024, None
    return None, "peak resident memory unavailable on this OS"


class Sampler:
    def __init__(self, proc):
        self.proc, self.peak, self.reason = proc, None, None
        self.stop = threading.Event()
        self.thread = threading.Thread(target=self._sample, daemon=True)
    def _sample(self):
        while not self.stop.is_set() and self.proc.poll() is None:
            value, reason = memory_reader(self.proc.pid)
            if value is not None: self.peak = max(value, self.peak or 0)
            elif self.reason is None: self.reason = reason
            self.stop.wait(.01)
    def __enter__(self): self.thread.start(); return self
    def __exit__(self, *_): self.stop.set(); self.thread.join(timeout=2)


def summary(samples, elapsed_s, memory_bytes=None, memory_reason=None):
    ms=[x*1000 for x in samples]
    return {"requests":len(ms), "elapsed_s":round(elapsed_s,4),
            "requests_per_sec":round(len(ms)/elapsed_s,2) if elapsed_s else None,
            "latency_ms":{"p50":round(percentile(ms,.5),3),"p95":round(percentile(ms,.95),3),
                           "p99":round(percentile(ms,.99),3),"max":round(max(ms),3)},
            "peak_resident_bytes":memory_bytes,"memory_unavailable_reason":memory_reason}


def capture_state(db):
    if not db.exists(): return {"complete":False,"session_count":0,"reason":"database missing"}
    conn=sqlite3.connect(db)
    try:
        rows=conn.execute("SELECT ended_at, dropped_messages, total_messages FROM sessions").fetchall()
    finally:
        conn.close()
    complete=len(rows)==1 and rows[0][0] is not None and rows[0][1]==0 and rows[0][2]>0
    return {"complete":complete,"session_count":len(rows),"ended":bool(rows and rows[0][0] is not None),
            "dropped_messages":rows[0][1] if rows else None,"total_messages":rows[0][2] if rows else None}


def stdio_run(binary, server, workdir, calls, warmup, message_bytes, proxied, index, delay_ms=0):
    suffix=f"-delay{delay_ms}" if delay_ms else ""
    db=workdir/f"stdio-{int(message_bytes)}-{proxied}-{index}{suffix}.db"
    cmd=[str(binary),"--db",str(db),"record","--client","bench-overhead","--",sys.executable,str(server)] if proxied else [sys.executable,str(server)]
    child_env=os.environ.copy()
    if delay_ms: child_env["FAKE_MCP_DELAY_MS"]=str(delay_ms)
    proc=subprocess.Popen(cmd,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.DEVNULL,cwd=server.parent.parent,env=child_env)
    samples=[]
    payload="x"*message_bytes
    with Sampler(proc) as sampler:
        try:
            handshake={"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"bench-overhead","version":"1"}}}
            proc.stdin.write(frame(handshake)); proc.stdin.flush()
            if not proc.stdout.readline(): raise RuntimeError("stdio benchmark server closed during initialize")
            for i in range(warmup):
                proc.stdin.write(frame({"jsonrpc":"2.0","id":i+2,"method":"tools/call","params":{"name":"echo","arguments":{"message":payload}}})); proc.stdin.flush(); proc.stdout.readline()
            start=time.perf_counter()
            for i in range(calls):
                msg={"jsonrpc":"2.0","id":i+warmup+10,"method":"tools/call","params":{"name":"echo","arguments":{"message":payload}}}
                t=time.perf_counter(); proc.stdin.write(frame(msg)); proc.stdin.flush(); line=proc.stdout.readline(); samples.append(time.perf_counter()-t)
                if not line: raise RuntimeError("stdio benchmark server closed mid-run")
                json.loads(line)
            elapsed=time.perf_counter()-start
        finally:
            proc.stdin.close()
            try: proc.wait(timeout=10)
            except subprocess.TimeoutExpired: proc.kill(); proc.wait()
    db_bytes=db.stat().st_size if db.exists() else 0
    wal=db.with_name(db.name+"-wal"); wal_bytes=wal.stat().st_size if wal.exists() else 0
    capture=capture_state(db) if proxied else {"complete":None,"reason":"direct server has no MCPTracer capture"}
    if proxied: require_complete_capture(capture,"stdio benchmark")
    return summary(samples,elapsed,sampler.peak,sampler.reason)|{"payload_bytes":message_bytes,"database_bytes":db_bytes,"wal_bytes":wal_bytes,"capture":capture}


def stdio_soak(binary, server, workdir, seconds):
    db=workdir/"stdio-soak.db"
    cmd=[str(binary),"--db",str(db),"record","--client","bench-soak","--",sys.executable,str(server)]
    proc=subprocess.Popen(cmd,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.DEVNULL,cwd=server.parent.parent)
    samples=[]; storage=[]
    try:
        proc.stdin.write(frame({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"bench-soak","version":"1"}}})); proc.stdin.flush()
        if not proc.stdout.readline(): raise RuntimeError("soak server closed during initialize")
        started=time.perf_counter(); deadline=started+seconds; ident=2; next_storage=started
        with Sampler(proc) as sampler:
            while time.perf_counter()<deadline:
                t=time.perf_counter(); proc.stdin.write(frame({"jsonrpc":"2.0","id":ident,"method":"tools/call","params":{"name":"echo","arguments":{"message":"soak"}}})); proc.stdin.flush()
                line=proc.stdout.readline(); samples.append(time.perf_counter()-t); ident+=1
                if not line: raise RuntimeError("soak server closed during run")
                if time.perf_counter()>=next_storage:
                    wal=Path(str(db)+"-wal")
                    storage.append({"elapsed_s":round(time.perf_counter()-started,2),"database_bytes":db.stat().st_size if db.exists() else 0,"wal_bytes":wal.stat().st_size if wal.exists() else 0})
                    next_storage=time.perf_counter()+1.0
            elapsed=time.perf_counter()-started
    finally:
        proc.stdin.close()
        try: proc.wait(timeout=10)
        except subprocess.TimeoutExpired: proc.kill(); proc.wait()
    capture=capture_state(db)
    require_complete_capture(capture,"stdio soak")
    return summary(samples,elapsed,sampler.peak,sampler.reason)|{"database_bytes":db.stat().st_size if db.exists() else 0,"wal_bytes":Path(str(db)+"-wal").stat().st_size if Path(str(db)+"-wal").exists() else 0,"capture":capture,"storage_growth_samples":storage}


def require_complete_capture(capture, label):
    if capture.get("complete") is not True:
        raise RuntimeError(f"{label} capture is incomplete: {capture}")


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1",0)); return sock.getsockname()[1]


def post(port, ident, method):
    conn=http.client.HTTPConnection("127.0.0.1",port,timeout=30)
    body=json.dumps({"jsonrpc":"2.0","id":ident,"method":method,"params":{}},separators=(",",":")).encode()
    headers={"Content-Type":"application/json","Accept":"application/json, text/event-stream","Mcp-Session-Id":"bench-session","MCP-Protocol-Version":"2025-06-18","Mcp-Method":method}
    conn.request("POST","/",body,headers); response=conn.getresponse(); data=response.read(); status=response.status; conn.close()
    if status!=200: raise RuntimeError(f"HTTP benchmark returned {status}: {data[:200]!r}")
    json.loads(data)


def http_run(binary, server, workdir, calls, warmup, proxied, index, concurrency=1, workload="sequential"):
    upstream_port, proxy_port=free_port(),free_port()
    upstream=subprocess.Popen([sys.executable,str(server),str(upstream_port)],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
    proxy=None; db=workdir/f"http-{proxied}-{workload}-{index}.db"
    try:
        deadline=time.time()+10
        while time.time()<deadline:
            try:
                with socket.create_connection(("127.0.0.1",upstream_port),timeout=.2): break
            except OSError: time.sleep(.05)
        else: raise RuntimeError("fake HTTP server did not start")
        port=upstream_port
        if proxied:
            proxy=subprocess.Popen([str(binary),"--db",str(db),"record-http","--listen",f"127.0.0.1:{proxy_port}","--target",f"http://127.0.0.1:{upstream_port}","--client","bench-http"],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,cwd=server.parent.parent)
            deadline=time.time()+10
            while time.time()<deadline:
                if proxy.poll() is not None: raise RuntimeError("record-http exited before accepting requests")
                try:
                    with socket.create_connection(("127.0.0.1",proxy_port),timeout=.2): break
                except OSError: time.sleep(.05)
            else: raise RuntimeError("record-http did not start")
            port=proxy_port
        target_process=proxy if proxied else upstream
        with Sampler(target_process) as sampler:
            post(port,1,"initialize")
            for i in range(warmup): post(port,i+2,"tools/list")
            identifiers=range(warmup+10,warmup+10+calls)
            def measured_post(ident):
                started=time.perf_counter()
                post(port,ident,"tools/list")
                return time.perf_counter()-started
            start=time.perf_counter()
            if concurrency == 1:
                samples=[measured_post(ident) for ident in identifiers]
            else:
                with ThreadPoolExecutor(max_workers=concurrency) as workers:
                    samples=list(workers.map(measured_post,identifiers))
            elapsed=time.perf_counter()-start
        if proxied:
            conn=http.client.HTTPConnection("127.0.0.1",port,timeout=10); conn.request("DELETE","/",headers={"Mcp-Session-Id":"bench-session"}); resp=conn.getresponse(); resp.read(); conn.close()
            proxy.terminate()
            try: proxy.wait(timeout=10)
            except subprocess.TimeoutExpired: proxy.kill(); proxy.wait()
        capture=capture_state(db) if proxied else {"complete":None,"reason":"direct server has no MCPTracer capture"}
        if proxied: require_complete_capture(capture,"HTTP benchmark")
        wal=Path(str(db)+"-wal")
        return summary(samples,elapsed,sampler.peak,sampler.reason)|{"database_bytes":db.stat().st_size if db.exists() else 0,"wal_bytes":wal.stat().st_size if wal.exists() else 0,"capture":capture}
    finally:
        if proxy:
            proxy.terminate()
            try: proxy.wait(timeout=5)
            except subprocess.TimeoutExpired: proxy.kill(); proxy.wait()
        upstream.terminate()
        try: upstream.wait(timeout=5)
        except subprocess.TimeoutExpired: upstream.kill(); upstream.wait()


def source_state_digest(repo, revision, excluded):
    digest=hashlib.sha256(revision.encode()+b"\0")
    diff=subprocess.run(["git","diff","--binary", "HEAD"],cwd=repo,capture_output=True,check=True).stdout
    digest.update(diff)
    others=subprocess.run(["git","ls-files","--others","--exclude-standard","-z"],cwd=repo,capture_output=True,check=True).stdout
    excluded={p.resolve() for p in excluded if p is not None}
    for raw in others.split(b"\0"):
        if not raw: continue
        path=repo/Path(os.fsdecode(raw))
        if path.resolve() in excluded: continue
        digest.update(raw+b"\0")
        digest.update(path.read_bytes())
    return digest.hexdigest()


def main():
    ap=argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--calls",type=int,default=50); ap.add_argument("--large-calls",type=int,default=10)
    ap.add_argument("--warmup",type=int,default=5); ap.add_argument("--repetitions",type=int,default=3)
    ap.add_argument("--large-bytes",type=int,default=262144); ap.add_argument("--soak-seconds",type=int,default=5)
    ap.add_argument("--concurrent-http",type=int,default=4,help="workers for the additional concurrent HTTP workload (2..32)")
    ap.add_argument("--slow-delay-ms",type=int,default=25); ap.add_argument("--output",type=Path)
    ap.add_argument("--json",action="store_true",help="print machine-readable JSON")
    a=ap.parse_args()
    if not (1<=a.calls<=1000 and 1<=a.large_calls<=100 and 0<=a.warmup<=100 and 1<=a.repetitions<=10 and 1024<=a.large_bytes<=4*1024*1024 and 1<=a.soak_seconds<=60 and 2<=a.concurrent_http<=32 and 1<=a.slow_delay_ms<=1000): ap.error("bounded values required: calls 1..1000, large-calls 1..100, warmup 0..100, repetitions 1..10, large-bytes 1KiB..4MiB, soak-seconds 1..60, concurrent-http 2..32, slow-delay-ms 1..1000")
    repo=Path(__file__).resolve().parent.parent; stdio_server=repo/"tests/fake_mcp_server.py"; http_server=repo/"tests/fake_mcp_http_server.py"
    subprocess.run(["cargo","build","--release","--locked","--bin","mcptracer"],cwd=repo,check=True)
    binary=repo/"target/release/mcptracer"; binary=binary if binary.exists() else binary.with_suffix(".exe")
    if not binary.exists(): ap.error(f"release binary not found: {binary}")
    with tempfile.TemporaryDirectory(prefix="mcptracer-bench-") as temp:
        work=Path(temp); runs={}
        for label,size,n in [("tiny_stdio",32,a.calls),("large_stdio",a.large_bytes,a.large_calls)]:
            for mode in ("direct","proxied"):
                values=[stdio_run(binary,stdio_server,work,n,a.warmup,size,mode=="proxied",i) for i in range(a.repetitions)]
                runs[f"{label}_{mode}"]=values
        for mode in ("direct","proxied"):
            values=[http_run(binary,http_server,work,a.calls,a.warmup,mode=="proxied",i) for i in range(a.repetitions)]
            runs[f"http_{mode}"]=values
            concurrent=[http_run(binary,http_server,work,a.calls,a.warmup,mode=="proxied",i,concurrency=a.concurrent_http,workload="concurrent") for i in range(a.repetitions)]
            runs[f"http_concurrent_{mode}"]=concurrent
        slow=[stdio_run(binary,stdio_server,work,a.calls,a.warmup,32,True,i,a.slow_delay_ms) for i in range(a.repetitions)]
        runs["controlled_slow_candidate_proxied"]=slow
        soak=stdio_soak(binary,stdio_server,work,a.soak_seconds)
        baseline_p50=statistics.median(x["latency_ms"]["p50"] for x in runs["tiny_stdio_proxied"])
        slow_p50=statistics.median(x["latency_ms"]["p50"] for x in slow)
        delta=round(slow_p50-baseline_p50,3)
        sensitivity={"injected_server_delay_ms":a.slow_delay_ms,"baseline_median_p50_ms":baseline_p50,"controlled_slow_median_p50_ms":slow_p50,"delta_ms":delta,"distinguishable":delta>=a.slow_delay_ms*0.5}
        if not sensitivity["distinguishable"]: raise RuntimeError(f"controlled slow candidate was not distinguishable: {sensitivity}")
        rev=subprocess.run(["git","rev-parse","HEAD"],cwd=repo,capture_output=True,text=True,check=True).stdout.strip()
        status=subprocess.run(["git","status","--porcelain"],cwd=repo,capture_output=True,text=True,check=True).stdout
        result={"schema":"mcptracer-benchmark.v1","provenance":{"source_revision":rev,"source_state_sha256":source_state_digest(repo,rev,[a.output]),"working_tree_dirty":bool(status),"binary_sha256":hashlib.sha256(binary.read_bytes()).hexdigest(),"build_profile":"release","features":"default","os":platform.platform(),"machine":platform.machine(),"cpu":platform.processor() or "unreported","python":platform.python_version(),"rustc":subprocess.run(["rustc","--version"],capture_output=True,text=True).stdout.strip()},"configuration":{"calls_per_run":a.calls,"large_calls_per_run":a.large_calls,"warmup_per_run":a.warmup,"repetitions":a.repetitions,"large_payload_bytes":a.large_bytes,"soak_seconds":a.soak_seconds,"concurrent_http_workers":a.concurrent_http,"controlled_slow_delay_ms":a.slow_delay_ms},"runs":runs,"soak":soak,"controlled_slow_sensitivity":sensitivity,"limitations":["Synthetic loopback fixtures only; the concurrent HTTP workload uses the configured worker count and is not production load.","Memory is sampled process working-set/high-water data when supported; otherwise null with a reason.","HTTP direct/proxied are loopback measurements; HTTP peak-memory polling can miss short-lived peaks.","The soak is bounded and synthetic; it is not multi-day reliability evidence."]}
        rendered=json.dumps(result,indent=2)
        if a.output: a.output.write_text(rendered+"\n",encoding="utf-8")
        print(rendered if a.json or not a.output else f"Wrote {a.output}")

if __name__=="__main__": main()

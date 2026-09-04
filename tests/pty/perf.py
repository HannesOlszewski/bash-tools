import sys, os, time, select; sys.path.insert(0,'tests/pty'); sys.path.insert(0,'/tmp/bt-exp')
import test_e2e as t
t.setUpModule()
b = t.ENV.bash()
log = os.path.join(t.ENV.dir, 'daemon.log')
line = 'ls -la /etc/passwd | grep root && echo "done $HOME" ; cat /etc/hosts'
lat = []; lost = []
for i, ch in enumerate(line):
    t0 = time.time()
    os.write(b.fd, ch.encode())
    ok = False
    while time.time() - t0 < 0.5:
        r,_,_ = select.select([b.fd],[],[],0.0002)
        if r:
            d = os.read(b.fd, 65536); b.raw += d; b.scr.feed(d)
        if b.scr.text(b.scr.cy) == ('$ ' + line[:i+1]).rstrip() and b.raw.endswith(b'\x1b8'):
            ok = True; break
    dt = (time.time() - t0) * 1000
    lat.append(dt)
    if not ok: lost.append((i, ch, dt))
    time.sleep(0.01)
lat.sort()
print("latency ms: min %.2f p50 %.2f p90 %.2f max %.2f" % (lat[0], lat[len(lat)//2], lat[int(len(lat)*.9)], lat[-1]))
print("lost:", lost)
b.close()
print(open(log).read()[-3000:])
t.tearDownModule()

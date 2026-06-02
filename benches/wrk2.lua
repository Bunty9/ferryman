-- wrk2 script for the ferryman P2 throughput bench.
--
-- Target (from projects-l3-l4.md § P2 eval):
--   wrk2 -c 1000 -t 16 -R 50000 -d 60s -s benches/wrk2.lua \
--        http://ferryman/svc-a/echo
--
-- Pass/fail thresholds:
--   p50  < 1ms
--   p99  < 5ms
--   p999 < 20ms
--   RSS  < 20MB at 50k rps idle
--
-- The script alternates between two routes so the round-robin between
-- upstreams is exercised. Override with `WRK_PATHS` if you wire more
-- routes later.

local paths = {
    "/svc-a/echo",
    "/svc-b/echo",
}
local idx = 0

request = function()
    idx = (idx % #paths) + 1
    return wrk.format("GET", paths[idx])
end

done = function(summary, latency, requests)
    io.write("------------------------------------------------------\n")
    io.write(string.format("requests:  %d\n", summary.requests))
    io.write(string.format("duration:  %.2fs\n", summary.duration / 1e6))
    io.write(string.format("errors:    %d\n", summary.errors.status + summary.errors.timeout))
    io.write(string.format("p50:       %.3fms\n", latency:percentile(50)  / 1000))
    io.write(string.format("p99:       %.3fms\n", latency:percentile(99)  / 1000))
    io.write(string.format("p999:      %.3fms\n", latency:percentile(99.9) / 1000))
    io.write(string.format("max:       %.3fms\n", latency.max / 1000))
end

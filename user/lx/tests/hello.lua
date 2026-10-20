-- Lua 5.4, built from github.com/lua/lua against musl, running on NodeOS.
print("hello from " .. _VERSION .. " on NodeOS")

-- closures, recursion, integers and floats
local function fib(n) if n < 2 then return n end return fib(n - 1) + fib(n - 2) end
print("fib(20) = " .. fib(20))
print(string.format("pi ~ %.6f, sqrt(2) = %.4f, 2^0.5 = %g", math.pi, math.sqrt(2), 2 ^ 0.5))
print("int division: 7 // 2 = " .. (7 // 2) .. ", float: " .. 7 / 2)

-- tables, sorting, string library
local words = {}
for word in ("the quick brown fox jumps over the lazy dog"):gmatch("%a+") do
  words[#words + 1] = word
end
table.sort(words)
print("sorted: " .. table.concat(words, " "))
print("gsub: " .. ("NodeOS"):gsub("O", "0"):upper())

-- coroutines and errors
local gen = coroutine.wrap(function() for i = 1, 3 do coroutine.yield(i * i) end end)
print("coroutine: " .. gen() .. " " .. gen() .. " " .. gen())
local ok, err = pcall(function() error("caught this") end)
print("pcall: " .. tostring(ok) .. " " .. err:match("caught this"))

-- a file round trip through the NodeOS filesystem service
local path = "/LUAOUT.TXT"
local f = assert(io.open(path, "w"))
for i = 1, 5 do f:write("line ", i, "\n") end
f:close()
local count, total = 0, 0
for line in io.lines(path) do
  count = count + 1
  total = total + tonumber(line:match("%d+"))
end
print(string.format("file: %d lines, sum %d", count, total))
local missing, msg = io.open("/no/such/file")
print("io.open missing: " .. tostring(missing) .. " (" .. msg .. ")")

-- garbage collection under allocation pressure
local t = {}
for i = 1, 200000 do t[i] = { i } end
t = nil
collectgarbage()
print(string.format("gc: %.0f KiB in use after collect", collectgarbage("count")))
print("os.clock() > 0: " .. tostring(os.clock() > 0))
print("done")

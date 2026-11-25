(module
  (import "wasi_snapshot_preview1" "fd_read"
    (func $read (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write"
    (func $write (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "proc_exit" (func $exit (param i32)))
  (memory (export "memory") 1)
  (data (i32.const 16) "ready\n")
  (func $read_input (result i32)
    (i32.store (i32.const 0) (i32.const 64))
    (i32.store (i32.const 4) (i32.const 4096))
    (if (call $read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 8))
      (then (call $exit (i32.const 70))))
    (i32.load (i32.const 8)))
  (func (export "_start") (local $count i32)
    (i32.store (i32.const 0) (i32.const 16))
    (i32.store (i32.const 4) (i32.const 6))
    (drop (call $write (i32.const 2) (i32.const 0) (i32.const 1) (i32.const 8)))
    (block $eof
      (loop $echo
        (local.set $count (call $read_input))
        (br_if $eof (i32.eqz (local.get $count)))
        (i32.store (i32.const 4) (local.get $count))
        (drop (call $write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 8)))
        (br $echo)))
    (if (call $read_input) (then (call $exit (i32.const 71))))
    (call $exit (i32.const 37))))

;; A small component that imports host functions and exports functions that use them.
;; build.rs also makes a larger copy by adding unused functions at the filler marker.
(component
    (import "double" (func $double (param "x" s32) (result s32)))
    (import "wait" (func $wait (param "ms" s32)))
    (core func $double_core (canon lower (func $double)))
    (core func $wait_core (canon lower (func $wait)))

    (core module $guest
        (import "host" "double" (func $double (param i32) (result i32)))
        (import "host" "wait" (func $wait (param i32)))
        (memory 1)

        ;; Returns double(x) + 1, so the result proves that both the host and guest ran.
        (func (export "run") (param i32) (result i32)
            local.get 0
            call $double
            i32.const 1
            i32.add)

        ;; Reads past the end of linear memory, which must trap.
        (func (export "read-out-of-bounds") (result i32)
            i32.const -4
            i32.load)

        ;; CPU-heavy work: hashes and rewrites all of linear memory on every round.
        (func $cpu (export "cpu") (param $rounds i32) (result i32)
            (local $address i32)
            (local $hash i32)
            i32.const 0x811c9dc5
            local.set $hash
            (block $done
                (loop $round
                    local.get $rounds
                    i32.eqz
                    br_if $done
                    i32.const 0
                    local.set $address
                    (loop $word
                        local.get $address
                        local.get $address
                        i32.load
                        local.get $hash
                        i32.xor
                        i32.const 5
                        i32.rotl
                        i32.const 16777619
                        i32.mul
                        local.tee $hash
                        i32.store
                        local.get $address
                        i32.const 4
                        i32.add
                        local.tee $address
                        i32.const 65536
                        i32.lt_u
                        br_if $word)
                    local.get $rounds
                    i32.const 1
                    i32.sub
                    local.set $rounds
                    br $round))
            local.get $hash)

        ;; Mostly waiting: asks the host to wait, then does a little work, like an agent
        ;; that waits for network replies.
        (func (export "wait-loop") (param $count i32) (param $ms i32) (result i32)
            (local $hash i32)
            (block $done
                (loop $again
                    local.get $count
                    i32.eqz
                    br_if $done
                    local.get $ms
                    call $wait
                    i32.const 1
                    call $cpu
                    local.set $hash
                    local.get $count
                    i32.const 1
                    i32.sub
                    local.set $count
                    br $again))
            local.get $hash)

        ;; @filler
    )

    (core instance $instance
        (instantiate $guest
            (with "host" (instance
                (export "double" (func $double_core))
                (export "wait" (func $wait_core))))))

    (func (export "run") (param "x" s32) (result s32)
        (canon lift (core func $instance "run")))
    (func (export "read-out-of-bounds") (result s32)
        (canon lift (core func $instance "read-out-of-bounds")))
    (func (export "cpu") (param "rounds" s32) (result s32)
        (canon lift (core func $instance "cpu")))
    (func (export "wait-loop") (param "count" s32) (param "ms" s32) (result s32)
        (canon lift (core func $instance "wait-loop"))))

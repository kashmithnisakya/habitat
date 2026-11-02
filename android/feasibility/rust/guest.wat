;; A small component that imports one host function and exports functions that use it.
(component
    (import "double" (func $double (param "x" s32) (result s32)))
    (core func $double_core (canon lower (func $double)))

    (core module $guest
        (import "host" "double" (func $double (param i32) (result i32)))
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
            i32.load))

    (core instance $instance
        (instantiate $guest
            (with "host" (instance (export "double" (func $double_core))))))

    (func (export "run") (param "x" s32) (result s32)
        (canon lift (core func $instance "run")))
    (func (export "read-out-of-bounds") (result s32)
        (canon lift (core func $instance "read-out-of-bounds"))))

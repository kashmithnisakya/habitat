//! Builds WebAssembly components that are small to send and expensive to compile. They're used to
//! measure what compiling can cost the host, and to test that it's limited.
//!
//! None of these has the agent's `handle-event` export, so even if one got through compiling it
//! could never run.

#![allow(dead_code)]

use wasm_encoder::{
    BlockType, CodeSection, ComponentBuilder, ComponentTypeRef, ComponentValType, ConstExpr,
    CustomSection, DataSection, ElementMode, ElementSection, Elements, EntityType, ExportKind,
    ExportSection, Function, FunctionSection, GlobalSection, GlobalType, ImportSection,
    InstanceType, MemorySection, MemoryType, Module, ModuleArg, NameMap, NameSection,
    PrimitiveValType, RefType, TableSection, TableType, TypeSection, ValType,
};

/// The most code that a function can have, which is a limit of Wasmtime's parser.
pub const MAX_WASM_FUNCTION_SIZE: usize = 7_654_321;

/// A module of functions that each have the same type and body.
fn module_of(count: usize, params: &[ValType], function: &Function) -> Module {
    let mut types = TypeSection::new();
    types.ty().function(params.iter().copied(), [ValType::I32]);
    let mut functions = FunctionSection::new();
    let mut code = CodeSection::new();
    for _ in 0..count {
        functions.function(0);
        code.function(function);
    }
    let mut module = Module::new();
    module.section(&types).section(&functions).section(&code);
    module
}

/// A component that holds a module and creates one instance of it.
pub fn component_of(module: &Module) -> Vec<u8> {
    let mut component = ComponentBuilder::default();
    let module = component.core_module(None, module);
    component.core_instantiate(None, module, Vec::<(&str, ModuleArg)>::new());
    component.finish()
}

/// A component that holds someone else's module without creating an instance of it, because the
/// module may have imports that nothing here could provide. Wasmtime compiles it all the same.
pub fn component_holding(module: &[u8]) -> Vec<u8> {
    let mut component = ComponentBuilder::default();
    component.core_module_raw(None, module);
    component.finish()
}

/// As many of the smallest possible functions as Wasmtime's parser allows.
pub fn tiny_functions(count: usize) -> Vec<u8> {
    component_of(&tiny_functions_module(count))
}

fn tiny_functions_module(count: usize) -> Module {
    let mut function = Function::new([]);
    function.instructions().i32_const(0).end();
    module_of(count, &[], &function)
}

/// Functions of plain arithmetic, which is close to the best case for the compiler per byte.
pub fn arithmetic(total_bytes: usize, function_bytes: usize) -> Vec<u8> {
    component_of(&arithmetic_module(total_bytes, function_bytes))
}

fn arithmetic_module(total_bytes: usize, function_bytes: usize) -> Module {
    let mut function = Function::new([]);
    {
        let mut body = function.instructions();
        body.local_get(0);
        for _ in 0..function_bytes.saturating_sub(16) / 3 {
            body.i32_const(7).i32_add();
        }
        body.end();
    }
    let count = (total_bytes / function_bytes.max(1)).max(1);
    module_of(count, &[ValType::I32], &function)
}

/// Functions that keep very many values alive at once, to make register allocation work hard.
pub fn many_live_values(total_bytes: usize, function_bytes: usize) -> Vec<u8> {
    let mut function = Function::new([]);
    {
        let mut body = function.instructions();
        let values = function_bytes.saturating_sub(16) / 3;
        for _ in 0..values {
            body.local_get(0);
        }
        for _ in 1..values {
            body.i32_add();
        }
        body.end();
    }
    let count = (total_bytes / function_bytes.max(1)).max(1);
    component_of(&module_of(count, &[ValType::I32], &function))
}

/// A function that's nothing but blocks inside blocks.
pub fn nested_blocks(depth: usize) -> Vec<u8> {
    let mut function = Function::new([]);
    {
        let mut body = function.instructions();
        for _ in 0..depth {
            body.block(BlockType::Empty);
        }
        for _ in 0..depth {
            body.end();
        }
        body.i32_const(0).end();
    }
    component_of(&module_of(1, &[], &function))
}

/// A function with many locals that are all used inside many nested loops. Each loop can change
/// each local, so the compiler has to track every local at every loop. The file grows with loops
/// plus locals, but the work grows with loops times locals.
pub fn loops_times_locals(loops: usize, locals: u32) -> Vec<u8> {
    let mut function = Function::new([(locals, ValType::I32)]);
    {
        let mut body = function.instructions();
        for _ in 0..loops {
            body.loop_(BlockType::Empty);
        }
        for local in 0..locals {
            body.local_get(local).local_set(local);
        }
        for _ in 0..loops {
            body.local_get(0).br_if(0).end();
        }
        body.i32_const(0).end();
    }
    component_of(&module_of(1, &[], &function))
}

/// Like `loops_times_locals`, without any loops: many blocks in a row, then every local is used.
pub fn blocks_times_locals(blocks: usize, locals: u32) -> Vec<u8> {
    let mut function = Function::new([(locals, ValType::I32)]);
    {
        let mut body = function.instructions();
        for _ in 0..blocks {
            body.block(BlockType::Empty)
                .local_get(0)
                .br_if(0)
                .local_get(0)
                .local_set(0)
                .end();
        }
        for local in 0..locals {
            body.local_get(local).drop();
        }
        body.i32_const(0).end();
    }
    component_of(&module_of(1, &[], &function))
}

/// Like `blocks_times_locals`, but the values being kept alive are on the operand stack, so there
/// aren't any locals to count.
pub fn blocks_times_stack(blocks: usize, values: usize) -> Vec<u8> {
    let mut function = Function::new([]);
    {
        let mut body = function.instructions();
        for _ in 0..values {
            body.local_get(0);
        }
        for _ in 0..blocks {
            body.block(BlockType::Empty).local_get(0).br_if(0).end();
        }
        for _ in 0..values {
            body.drop();
        }
        body.i32_const(0).end();
    }
    component_of(&module_of(1, &[ValType::I32], &function))
}

/// Blocks, loops or `if`s in a row that each take values as parameters and pass them on, so that
/// every value has to be tracked through every one of them, again without any locals.
pub fn control_with_params(kind: usize, count: usize, values: usize) -> Vec<u8> {
    let mut types = TypeSection::new();
    types.ty().function([ValType::I32], [ValType::I32]);
    types
        .ty()
        .function(vec![ValType::I32; values], vec![ValType::I32; values]);
    let mut functions = FunctionSection::new();
    functions.function(0);
    let mut function = Function::new([]);
    {
        let mut body = function.instructions();
        for _ in 0..values {
            body.local_get(0);
        }
        let ty = BlockType::FunctionType(1);
        for _ in 0..count {
            match kind {
                0 => {
                    body.loop_(ty);
                }
                1 => {
                    body.block(ty);
                }
                _ => {
                    body.local_get(0).if_(ty).else_();
                }
            }
            body.end();
        }
        for _ in 0..values {
            body.drop();
        }
        body.i32_const(0).end();
    }
    let mut code = CodeSection::new();
    code.function(&function);
    let mut module = Module::new();
    module.section(&types).section(&functions).section(&code);
    component_of(&module)
}

/// Loops in a row, some nested, that each use every local. Unlike `loops_times_locals`, the file
/// pays for each use, so this shows the cost when nesting is kept shallow.
pub fn flat_loops_times_locals(
    functions: usize,
    loops: usize,
    locals: u32,
    depth: usize,
) -> Vec<u8> {
    component_of(&flat_loops_module(functions, loops, locals, depth))
}

fn flat_loops_module(functions: usize, loops: usize, locals: u32, depth: usize) -> Module {
    let mut function = Function::new([(locals, ValType::I32)]);
    {
        let mut body = function.instructions();
        for _ in 0..loops / depth.max(1) {
            for _ in 0..depth {
                body.loop_(BlockType::Empty);
            }
            for local in 0..locals {
                body.local_get(local)
                    .i32_const(1)
                    .i32_add()
                    .local_set(local);
            }
            for _ in 0..depth {
                body.local_get(0).br_if(0).end();
            }
        }
        body.i32_const(0).end();
    }
    module_of(functions, &[], &function)
}

/// Many functions that each declare many locals, which cost five bytes to declare but which the
/// compiler has to set to zero one at a time.
pub fn functions_with_locals(count: usize, locals: u32) -> Vec<u8> {
    let mut function = Function::new([(locals, ValType::I32)]);
    function.instructions().i32_const(0).end();
    component_of(&module_of(count, &[], &function))
}

/// Many exported functions of one type with many parameters and results. Each export gets glue
/// code from Wasmtime to move those between the host and WebAssembly.
pub fn exported_functions_with_params(count: u32, params: usize) -> Vec<u8> {
    let mut types = TypeSection::new();
    types
        .ty()
        .function(vec![ValType::I32; params], vec![ValType::I32; params]);
    let mut functions = FunctionSection::new();
    let mut exports = ExportSection::new();
    let mut code = CodeSection::new();
    let mut function = Function::new([]);
    function.instructions().unreachable().end();
    for index in 0..count {
        functions.function(0);
        exports.export(&format!("f{index}"), ExportKind::Func, index);
        code.function(&function);
    }
    let mut module = Module::new();
    module
        .section(&types)
        .section(&functions)
        .section(&exports)
        .section(&code);
    component_of(&module)
}

/// Many functions of one type with many parameters and results, listed in a table rather than
/// exported, so that there's no limit from the size of the module's type. Each still gets glue code
/// from Wasmtime to move those values between the host and WebAssembly.
pub fn tabled_functions_with_params(count: u32, params: usize) -> Vec<u8> {
    let mut types = TypeSection::new();
    types
        .ty()
        .function(vec![ValType::I32; params], vec![ValType::I32; params]);
    let mut functions = FunctionSection::new();
    let mut code = CodeSection::new();
    let mut function = Function::new([]);
    function.instructions().unreachable().end();
    let indexes: Vec<u32> = (0..count).collect();
    for _ in 0..count {
        functions.function(0);
        code.function(&function);
    }
    let mut elements = ElementSection::new();
    elements.declared(Elements::Functions(std::borrow::Cow::Borrowed(&indexes)));
    let mut module = Module::new();
    module
        .section(&types)
        .section(&functions)
        .section(&elements)
        .section(&code);
    component_of(&module)
}

/// Many different function types with many parameters, and nothing using them. Wasmtime still
/// prepares glue code for each.
pub fn many_function_types(count: u32, params: usize) -> Vec<u8> {
    let mut types = TypeSection::new();
    for index in 0..count {
        // The first few parameters are what make each type different.
        let mut values = vec![ValType::I32; params];
        for (bit, value) in values.iter_mut().take(32).enumerate() {
            if index >> bit & 1 == 1 {
                *value = ValType::I64;
            }
        }
        types.ty().function(values, []);
    }
    let mut module = Module::new();
    module.section(&types);
    component_of(&module)
}

/// Many types that each describe an instance with very many functions, which count as one
/// declaration each.
pub fn huge_component_types(count: usize, functions: u32) -> Vec<u8> {
    let mut ty = InstanceType::new();
    ty.ty()
        .function()
        .params(Vec::<(&str, ComponentValType)>::new())
        .result(None);
    for index in 0..functions {
        ty.export(format!("f{index}"), ComponentTypeRef::Func(0));
    }
    let mut component = ComponentBuilder::default();
    for _ in 0..count {
        component.type_instance(None, &ty);
    }
    component.finish()
}

/// A name for every function, which fills a custom section that Wasmtime reads.
pub fn long_function_names(count: u32, name_bytes: usize) -> Vec<u8> {
    let mut function = Function::new([]);
    function.instructions().i32_const(0).end();
    let mut module = module_of(count as usize, &[], &function);
    let name = "x".repeat(name_bytes);
    let mut names = NameMap::new();
    for index in 0..count {
        names.append(index, &name);
    }
    let mut section = NameSection::new();
    section.functions(&names);
    module.section(&section);
    component_of(&module)
}

/// A function with one branch that has very many targets, inside many blocks.
pub fn br_table(targets: usize, depth: u32) -> Vec<u8> {
    let mut function = Function::new([]);
    {
        let mut body = function.instructions();
        for _ in 0..depth {
            body.block(BlockType::Empty);
        }
        body.local_get(0);
        let labels: Vec<u32> = (0..targets as u32).map(|target| target % depth).collect();
        body.br_table(labels, 0);
        for _ in 0..depth {
            body.end();
        }
        body.i32_const(0).end();
    }
    component_of(&module_of(1, &[ValType::I32], &function))
}

/// One memory with one data segment, which has to be copied rather than compiled.
pub fn data_segment(bytes: usize) -> Vec<u8> {
    component_of(&data_module(bytes))
}

fn data_module(bytes: usize) -> Module {
    let mut memories = MemorySection::new();
    memories.memory(memory_type(((bytes / 65536) + 1) as u64));
    let mut data = DataSection::new();
    data.active(
        0,
        &ConstExpr::i32_const(0),
        std::iter::repeat_n(0xabu8, bytes),
    );
    let mut module = Module::new();
    module.section(&memories).section(&data);
    module
}

/// One memory with very many small data segments.
pub fn many_data_segments(count: u32) -> Vec<u8> {
    let mut memories = MemorySection::new();
    memories.memory(memory_type(1024));
    let mut data = DataSection::new();
    for index in 0..count {
        data.active(0, &ConstExpr::i32_const((index * 8) as i32), [1u8]);
    }
    let mut module = Module::new();
    module.section(&memories).section(&data);
    component_of(&module)
}

fn memory_type(pages: u64) -> MemoryType {
    MemoryType {
        minimum: pages,
        maximum: None,
        memory64: false,
        shared: false,
        page_size_log2: None,
    }
}

/// Modules of memories whose data has a single byte far apart, so that a few bytes ask Wasmtime to
/// prepare a large image of each memory.
pub fn sparse_memory_images(modules: usize, memories_per_module: u32, gap: u32) -> Vec<u8> {
    let pages = u64::from(gap / 65536 + 2);
    let mut section = MemorySection::new();
    let mut data = DataSection::new();
    for memory in 0..memories_per_module {
        section.memory(memory_type(pages));
        data.active(memory, &ConstExpr::i32_const(0), [1u8]);
        data.active(memory, &ConstExpr::i32_const(gap as i32), [1u8]);
    }
    let mut module = Module::new();
    module.section(&section).section(&data);

    let mut component = ComponentBuilder::default();
    for _ in 0..modules {
        let index = component.core_module(None, &module);
        component.core_instantiate(None, index, Vec::<(&str, ModuleArg)>::new());
    }
    component.finish()
}

/// A table with a single function far from the start, which Wasmtime may lay out in advance.
pub fn sparse_tables(modules: usize, tables_per_module: u32, offset: u32) -> Vec<u8> {
    let mut types = TypeSection::new();
    types.ty().function([], []);
    let mut functions = FunctionSection::new();
    functions.function(0);
    let mut tables = TableSection::new();
    let mut elements = ElementSection::new();
    for table in 0..tables_per_module {
        tables.table(TableType {
            element_type: RefType::FUNCREF,
            minimum: u64::from(offset) + 1,
            maximum: None,
            table64: false,
            shared: false,
        });
        elements.segment(wasm_encoder::ElementSegment {
            mode: ElementMode::Active {
                table: Some(table),
                offset: &ConstExpr::i32_const(offset as i32),
            },
            elements: Elements::Functions(std::borrow::Cow::Borrowed(&[0])),
        });
    }
    let mut code = CodeSection::new();
    let mut function = Function::new([]);
    function.instructions().end();
    code.function(&function);
    let mut module = Module::new();
    module
        .section(&types)
        .section(&functions)
        .section(&tables)
        .section(&elements)
        .section(&code);

    let mut component = ComponentBuilder::default();
    for _ in 0..modules {
        let index = component.core_module(None, &module);
        component.core_instantiate(None, index, Vec::<(&str, ModuleArg)>::new());
    }
    component.finish()
}

/// A long list of function types, globals and exports, which need no code to be compiled.
pub fn many_declarations(count: u32) -> Vec<u8> {
    let mut types = TypeSection::new();
    let mut functions = FunctionSection::new();
    let mut code = CodeSection::new();
    let mut globals = GlobalSection::new();
    let mut exports = ExportSection::new();
    let mut function = Function::new([]);
    function.instructions().end();
    for index in 0..count {
        types.ty().function([], []);
        globals.global(
            GlobalType {
                val_type: ValType::I32,
                mutable: false,
                shared: false,
            },
            &ConstExpr::i32_const(0),
        );
        exports.export(&format!("g{index}"), ExportKind::Global, index);
    }
    functions.function(0);
    code.function(&function);
    let mut module = Module::new();
    module
        .section(&types)
        .section(&functions)
        .section(&globals)
        .section(&exports)
        .section(&code);
    component_of(&module)
}

/// A large custom section, which should cost next to nothing.
pub fn custom_section(bytes: usize) -> Vec<u8> {
    let data = vec![0xabu8; bytes];
    let mut module = Module::new();
    module.section(&CustomSection {
        name: "padding".into(),
        data: data.into(),
    });
    component_of(&module)
}

/// Many separate modules, each compiled on its own, with one function each.
pub fn many_modules(modules: usize) -> Vec<u8> {
    let mut function = Function::new([]);
    function.instructions().i32_const(0).end();
    let module = module_of(1, &[], &function);
    let mut component = ComponentBuilder::default();
    for _ in 0..modules {
        let index = component.core_module(None, &module);
        component.core_instantiate(None, index, Vec::<(&str, ModuleArg)>::new());
    }
    component.finish()
}

/// One module with very many instances, each of which needs its own data structures.
pub fn many_instances(instances: usize) -> Vec<u8> {
    let mut component = ComponentBuilder::default();
    let module = component.core_module(None, &Module::new());
    for _ in 0..instances {
        component.core_instantiate(None, module, Vec::<(&str, ModuleArg)>::new());
    }
    component.finish()
}

/// Very many instances of one module with very many exports, or imports that each instance has to
/// be given. No components are nested here, so Wasmtime still has to do this much work per file.
pub fn instances_times_imports(instances: usize, imports: u32, exports: u32) -> Vec<u8> {
    let mut types = TypeSection::new();
    types.ty().function([], []);
    let mut section = ImportSection::new();
    for index in 0..imports {
        section.import("a", &format!("f{index}"), EntityType::Function(0));
    }
    let mut functions = FunctionSection::new();
    let mut export_section = ExportSection::new();
    let mut code = CodeSection::new();
    let mut function = Function::new([]);
    function.instructions().end();
    functions.function(0);
    code.function(&function);
    for index in 0..exports {
        export_section.export(&format!("e{index}"), ExportKind::Func, imports);
    }
    let mut module = Module::new();
    module
        .section(&types)
        .section(&section)
        .section(&functions)
        .section(&export_section)
        .section(&code);

    // Something for the module to import, which has one function under every name.
    let mut types = TypeSection::new();
    types.ty().function([], []);
    let mut functions = FunctionSection::new();
    functions.function(0);
    let mut exports = ExportSection::new();
    for index in 0..imports {
        exports.export(&format!("f{index}"), ExportKind::Func, 0);
    }
    let mut code = CodeSection::new();
    let mut function = Function::new([]);
    function.instructions().end();
    code.function(&function);
    let mut provider = Module::new();
    provider
        .section(&types)
        .section(&functions)
        .section(&exports)
        .section(&code);

    let mut component = ComponentBuilder::default();
    let provider = component.core_module(None, &provider);
    let provider = component.core_instantiate(None, provider, Vec::<(&str, ModuleArg)>::new());
    let module = component.core_module(None, &module);
    for _ in 0..instances {
        component.core_instantiate(None, module, [("a", ModuleArg::Instance(provider))]);
    }
    component.finish()
}

/// Blocks in a row that each produce one value, which is about as cheap as a block can be in the
/// file and which real code does use.
pub fn blocks_with_a_result(count: usize) -> Vec<u8> {
    let mut function = Function::new([]);
    {
        let mut body = function.instructions();
        for _ in 0..count {
            body.block(BlockType::Result(ValType::I32))
                .local_get(0)
                .end()
                .drop();
        }
        body.i32_const(0).end();
    }
    component_of(&module_of(1, &[ValType::I32], &function))
}

/// The costliest things that can be done within all of the default limits, in one component. These
/// are all at once, except for what they have to share: code, and work.
pub fn everything_at_the_limits() -> Vec<u8> {
    const KB: usize = 1024;
    let mut component = ComponentBuilder::default();
    let modules = [
        arithmetic_module(15_800 * KB, 512 * KB - 64),
        flat_loops_module(1, 700, 700, 16),
        tiny_functions_module(99_000),
        data_module(90 * 1024 * KB),
    ];
    for module in &modules {
        let module = component.core_module(None, module);
        component.core_instantiate(None, module, Vec::<(&str, ModuleArg)>::new());
    }

    // Instances of a module with many imports, as in `instances_times_imports`.
    let mut types = TypeSection::new();
    types.ty().function([], []);
    let mut functions = FunctionSection::new();
    functions.function(0);
    let mut exports = ExportSection::new();
    let mut imports = ImportSection::new();
    for index in 0..15_000 {
        exports.export(&format!("f{index}"), ExportKind::Func, 0);
        imports.import("a", &format!("f{index}"), EntityType::Function(0));
    }
    let mut code = CodeSection::new();
    let mut function = Function::new([]);
    function.instructions().end();
    code.function(&function);
    let mut provider = Module::new();
    provider
        .section(&types)
        .section(&functions)
        .section(&exports)
        .section(&code);
    let provider = component.core_module(None, &provider);
    let provider = component.core_instantiate(None, provider, Vec::<(&str, ModuleArg)>::new());
    let mut user = Module::new();
    user.section(&types).section(&imports);
    let user = component.core_module(None, &user);
    for _ in 0..250 {
        component.core_instantiate(None, user, [("a", ModuleArg::Instance(provider))]);
    }
    component.finish()
}

/// Components inside components, with nothing else in them.
pub fn nested_components(depth: usize) -> Vec<u8> {
    let mut component = ComponentBuilder::default();
    for _ in 0..depth {
        let mut outer = ComponentBuilder::default();
        outer.component(None, component);
        component = outer;
    }
    component.finish()
}

/// Components that each create several instances of the one inside them. Wasmtime works out every
/// instance when it compiles, so a few hundred bytes ask for `width` to the power of `depth`.
pub fn instantiation_bomb(depth: usize, width: usize) -> Vec<u8> {
    let mut inner = ComponentBuilder::default();
    let module = inner.core_module(None, &Module::new());
    inner.core_instantiate(None, module, Vec::<(&str, ModuleArg)>::new());
    for _ in 0..depth {
        let mut outer = ComponentBuilder::default();
        let child = outer.component(None, inner);
        for _ in 0..width {
            outer.instantiate(
                None,
                child,
                Vec::<(&str, wasm_encoder::ComponentExportKind, u32)>::new(),
            );
        }
        inner = outer;
    }
    inner.finish()
}

/// Many functions that the host would have to provide, each needing its own glue code. Every one
/// imports the same module and core function type, so nearly all the cost is the component layer.
pub fn many_lowered_imports(count: u32) -> Vec<u8> {
    let mut component = ComponentBuilder::default();
    let (ty, mut encoder) = component.type_function(None);
    encoder
        .params([("a", PrimitiveValType::String)])
        .result(None);
    let imported = component.import("log", ComponentTypeRef::Func(ty));

    // A module to provide the memory that lowering a string needs.
    let mut memories = MemorySection::new();
    memories.memory(memory_type(1));
    let mut exports = ExportSection::new();
    exports.export("memory", ExportKind::Memory, 0);
    let mut module = Module::new();
    module.section(&memories).section(&exports);
    let module = component.core_module(None, &module);
    let instance = component.core_instantiate(None, module, Vec::<(&str, ModuleArg)>::new());
    let memory = component.core_alias_export(None, instance, "memory", ExportKind::Memory);

    for _ in 0..count {
        component.lower_func(
            None,
            imported,
            [
                wasm_encoder::CanonicalOption::Memory(memory),
                wasm_encoder::CanonicalOption::UTF8,
            ],
        );
    }
    component.finish()
}

/// Two modules joined by the component layer, with a different function for each call. Wasmtime
/// generates and compiles a function to adapt each one, which is code the package doesn't hold.
///
/// Above a depth of 0, the value passed is a tuple of tuples, which doubles in size with each level
/// and which every adapter has to copy field by field.
pub fn many_adapters(count: u32, depth: u32) -> Vec<u8> {
    let mut component = ComponentBuilder::default();
    let core_params: &[ValType] = if depth == 0 {
        &[ValType::I32, ValType::I32]
    } else {
        &[ValType::I32]
    };

    // The side that's called, with its memory and allocator.
    let mut types = TypeSection::new();
    types.ty().function(core_params.iter().copied(), []);
    types.ty().function([ValType::I32; 4], [ValType::I32]);
    let mut functions = FunctionSection::new();
    let mut code = CodeSection::new();
    let mut exports = ExportSection::new();
    let mut f = Function::new([]);
    f.instructions().end();
    for index in 0..count {
        functions.function(0);
        code.function(&f);
        exports.export(&format!("f{index}"), ExportKind::Func, index);
    }
    functions.function(1);
    let mut realloc = Function::new([]);
    realloc.instructions().i32_const(0).end();
    code.function(&realloc);
    exports.export("realloc", ExportKind::Func, count);
    exports.export("memory", ExportKind::Memory, 0);
    let mut memories = MemorySection::new();
    memories.memory(memory_type(1));
    let mut callee = Module::new();
    callee
        .section(&types)
        .section(&functions)
        .section(&memories)
        .section(&exports)
        .section(&code);
    let callee = component.core_module(None, &callee);
    let callee = component.core_instantiate(None, callee, Vec::<(&str, ModuleArg)>::new());
    let core_realloc = component.core_alias_export(None, callee, "realloc", ExportKind::Func);
    let core_memory = component.core_alias_export(None, callee, "memory", ExportKind::Memory);

    // The side that calls, with a memory of its own.
    let mut memories = MemorySection::new();
    memories.memory(memory_type(1));
    let mut exports = ExportSection::new();
    exports.export("memory", ExportKind::Memory, 0);
    let mut caller = Module::new();
    caller.section(&memories).section(&exports);
    let caller = component.core_module(None, &caller);
    let caller = component.core_instantiate(None, caller, Vec::<(&str, ModuleArg)>::new());
    let caller_memory = component.core_alias_export(None, caller, "memory", ExportKind::Memory);

    let mut param = ComponentValType::Primitive(if depth == 0 {
        PrimitiveValType::String
    } else {
        PrimitiveValType::U32
    });
    for _ in 0..depth {
        let (tuple, encoder) = component.type_defined(None);
        encoder.tuple([param, param]);
        param = ComponentValType::Type(tuple);
    }
    let (ty, mut encoder) = component.type_function(None);
    encoder.params([("a", param)]).result(None);

    // Each of these has to be used for its adapter to be generated, so a module takes them all.
    let mut lowered = Vec::new();
    for index in 0..count {
        let core_f =
            component.core_alias_export(None, callee, &format!("f{index}"), ExportKind::Func);
        let lifted = component.lift_func(
            None,
            core_f,
            ty,
            [
                wasm_encoder::CanonicalOption::Memory(core_memory),
                wasm_encoder::CanonicalOption::Realloc(core_realloc),
                wasm_encoder::CanonicalOption::UTF8,
            ],
        );
        lowered.push(component.lower_func(
            None,
            lifted,
            [
                wasm_encoder::CanonicalOption::Memory(caller_memory),
                wasm_encoder::CanonicalOption::UTF16,
            ],
        ));
    }
    let mut types = TypeSection::new();
    types.ty().function(core_params.iter().copied(), []);
    let mut imports = ImportSection::new();
    for index in 0..count {
        imports.import("a", &format!("f{index}"), EntityType::Function(0));
    }
    let mut user = Module::new();
    user.section(&types).section(&imports);
    let user = component.core_module(None, &user);
    let names: Vec<String> = (0..count).map(|index| format!("f{index}")).collect();
    let args = component.core_instantiate_exports(
        None,
        names
            .iter()
            .zip(&lowered)
            .map(|(name, func)| (name.as_str(), ExportKind::Func, *func)),
    );
    component.core_instantiate(None, user, [("a", ModuleArg::Instance(args))]);
    component.finish()
}

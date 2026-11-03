//! Works out what a component would ask of the compiler, without compiling it.
//!
//! This reads a package's code as untrusted input, so it uses the same parser and validator that
//! Wasmtime goes on to use, which means the two can't disagree about what the bytes hold.

use wasmtime::wasmparser::{
    BinaryReaderError, BlockType, CompositeInnerType, FuncValidatorAllocations, FunctionBody,
    Operator, Parser, Payload, ValidPayload, Validator, ValidatorResources, WasmFeatures,
    WasmModuleResources,
};

/// What a component holds, as far as compiling it is concerned.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CodeShape {
    /// Components inside the component. Wasmtime works out every instance of these when it
    /// compiles, so their cost multiplies with each level.
    pub nested_components: u64,
    pub modules: u64,
    /// Instances of modules. Wasmtime records every import of every one of them.
    pub module_instances: u64,
    /// Imports of all modules.
    pub module_imports: u64,
    pub functions: u64,
    /// The size of all function bodies.
    pub code_bytes: u64,
    /// The size of the largest function body.
    pub largest_function: u64,
    /// The work that all functions together make for the compiler. See `read_function`.
    pub work: u64,
    /// The deepest that loops are nested in any function.
    pub deepest_loops: u64,
    /// The most loops in any function.
    pub most_loops: u64,
    /// The most branch points in any function.
    pub most_branch_points: u64,
    /// The most values passed through the blocks and loops of any function. Real code hardly does
    /// this, and it costs the compiler memory that grows with these values times the blocks.
    pub most_block_values: u64,
    pub memories: u64,
    /// The starting size of all tables, which Wasmtime can lay out in advance.
    pub table_entries: u64,
    /// Functions moved between the component and module layers, which each need generated code.
    pub canonical_functions: u64,
    /// Types, imports, exports, instances and the like, of which there can be very many.
    pub declarations: u64,
    /// The size of all of those. A single declaration can be very large, such as the type of an
    /// instance with many functions.
    pub declaration_bytes: u64,
    /// Parameters and results in function types, and in the functions that use them. Wasmtime
    /// generates code to move each of them between the host and WebAssembly.
    pub signature_values: u64,
}

impl CodeShape {
    /// Reads a whole component. The error is the parser's own, and can hold text from it.
    pub fn of(component: &[u8]) -> Result<Self, BinaryReaderError> {
        Self::read(component, |_| true)
    }

    /// Reads a component, stopping as soon as `keep_going` returns false. The shape then shows
    /// what made it stop, and what comes later in the component hasn't been read or checked.
    pub(crate) fn read(
        component: &[u8],
        mut keep_going: impl FnMut(&Self) -> bool,
    ) -> Result<Self, BinaryReaderError> {
        let mut shape = Self::default();
        let mut validator = Validator::new_with_features(WasmFeatures::all());
        let mut allocations = FuncValidatorAllocations::default();
        for payload in Parser::new(0).parse_all(component) {
            let payload = payload?;
            // Before anything spends time on what this part of the component holds.
            shape.count(&payload)?;
            if !keep_going(&shape) {
                return Ok(shape);
            }
            if let ValidPayload::Func(function, body) = validator.payload(&payload)? {
                let mut function = function.into_validator(allocations);
                shape.read_function(&mut function, &body)?;
                allocations = function.into_allocations();
                if !keep_going(&shape) {
                    return Ok(shape);
                }
            }
        }
        Ok(shape)
    }

    fn count(&mut self, payload: &Payload<'_>) -> Result<(), BinaryReaderError> {
        let (declarations, range) = match payload {
            Payload::ModuleSection { .. } => {
                self.modules += 1;
                return Ok(());
            }
            Payload::ComponentSection { .. } => {
                self.nested_components += 1;
                return Ok(());
            }
            Payload::CodeSectionStart { size, .. } => {
                self.code_bytes += u64::from(*size);
                return Ok(());
            }
            Payload::CodeSectionEntry(body) => {
                let bytes = body.range().end - body.range().start;
                self.largest_function = self.largest_function.max(bytes);
                return Ok(());
            }
            Payload::InstanceSection(section) => {
                self.module_instances += u64::from(section.count());
                (section.count(), section.range())
            }
            Payload::ImportSection(section) => {
                self.module_imports += u64::from(section.count());
                (section.count(), section.range())
            }
            Payload::FunctionSection(section) => {
                self.functions += u64::from(section.count());
                (0, section.range())
            }
            Payload::MemorySection(section) => {
                self.memories += u64::from(section.count());
                (0, section.range())
            }
            Payload::TableSection(section) => {
                for table in section.clone() {
                    self.table_entries = self.table_entries.saturating_add(table?.ty.initial);
                }
                (0, section.range())
            }
            Payload::ComponentCanonicalSection(section) => {
                self.canonical_functions += u64::from(section.count());
                (0, section.range())
            }
            Payload::TypeSection(section) => {
                for group in section.clone() {
                    for ty in group?.into_types() {
                        if let CompositeInnerType::Func(ty) = &ty.composite_type.inner {
                            let values = ty.params().len() + ty.results().len();
                            self.signature_values += values as u64;
                        }
                    }
                }
                (section.count(), section.range())
            }
            Payload::GlobalSection(section) => (section.count(), section.range()),
            Payload::ExportSection(section) => (section.count(), section.range()),
            Payload::ElementSection(section) => (section.count(), section.range()),
            Payload::TagSection(section) => (section.count(), section.range()),
            Payload::CoreTypeSection(section) => (section.count(), section.range()),
            Payload::ComponentInstanceSection(section) => (section.count(), section.range()),
            Payload::ComponentAliasSection(section) => (section.count(), section.range()),
            Payload::ComponentTypeSection(section) => (section.count(), section.range()),
            Payload::ComponentImportSection(section) => (section.count(), section.range()),
            Payload::ComponentExportSection(section) => (section.count(), section.range()),
            // Data only has to be copied, which is limited by the size of the package.
            Payload::DataSection(section) => {
                self.declarations += u64::from(section.count());
                return Ok(());
            }
            _ => return Ok(()),
        };
        self.declarations += u64::from(declarations);
        self.declaration_bytes += range.end - range.start;
        Ok(())
    }

    /// Checks one function, and notes how much work it would make for the compiler.
    fn read_function(
        &mut self,
        function: &mut wasmtime::wasmparser::FuncValidator<ValidatorResources>,
        body: &FunctionBody<'_>,
    ) -> Result<(), BinaryReaderError> {
        let mut locals = body.get_locals_reader()?;
        for _ in 0..locals.get_count() {
            let offset = locals.original_position();
            let (count, ty) = locals.read()?;
            function.define_locals(offset, count, ty)?;
        }

        let mut operators = body.get_operators_reader()?;
        let mut stack_height = 0;
        let mut branch_points = 0u64;
        let mut loops = 0u64;
        let mut block_values = 0u64;
        // For each block that's open, whether it's a loop.
        let mut open_blocks = Vec::new();
        let mut open_loops = 0u64;
        let mut deepest_loops = 0;
        while !operators.eof() {
            let (operator, offset) = operators.read_with_offset()?;
            // First, as this is what makes sure that the operator's indexes are in range.
            function.op(offset, &operator)?;
            stack_height = stack_height.max(function.operand_stack_height());

            let block_type = match &operator {
                Operator::Block { blockty }
                | Operator::Loop { blockty }
                | Operator::If { blockty } => Some(*blockty),
                Operator::TryTable { try_table } => Some(try_table.ty),
                _ => None,
            };
            if let Some(block_type) = block_type {
                branch_points += 1;
                let is_loop = matches!(operator, Operator::Loop { .. });
                open_blocks.push(is_loop);
                if is_loop {
                    loops += 1;
                    open_loops += 1;
                    deepest_loops = deepest_loops.max(open_loops);
                }
                match block_type {
                    BlockType::Empty => {}
                    BlockType::Type(_) => block_values += 1,
                    BlockType::FuncType(index) => {
                        let resources = function.resources();
                        if let Some(ty) = resources.sub_type_at(index) {
                            let ty = ty.unwrap_func();
                            block_values += (ty.params().len() + ty.results().len()) as u64;
                        }
                    }
                }
            }
            match &operator {
                Operator::End => {
                    if open_blocks.pop() == Some(true) {
                        open_loops -= 1;
                    }
                }
                Operator::Else
                | Operator::BrIf { .. }
                | Operator::BrOnNull { .. }
                | Operator::BrOnNonNull { .. }
                | Operator::BrOnCast { .. }
                | Operator::BrOnCastFail { .. } => branch_points += 1,
                Operator::BrTable { targets } => branch_points += u64::from(targets.len()) + 1,
                _ => {}
            }
        }

        // The compiler tracks each value at each point where the path through a function can
        // branch or join. A function also pays for its results, which it has to hand back. The
        // time this takes grows faster than the number of those pairs, with hostile code taking
        // up to four times as long for twice as many, so it's the square of that which adds up.
        // That way, a few very complex functions can't add up to much more than one.
        let resources = function.resources();
        let signature = resources
            .type_id_of_function(function.index())
            .map(|id| resources.sub_type_at_id(id).unwrap_func());
        let params = signature.map_or(0, |ty| ty.params().len() as u64);
        let results = signature.map_or(0, |ty| ty.results().len() as u64);
        self.signature_values += params + results;
        let values = u64::from(function.len_locals()) + u64::from(stack_height) + results;
        let pairs = values.saturating_mul(branch_points + 1);
        self.work = self.work.saturating_add(pairs.saturating_mul(pairs));
        self.deepest_loops = self.deepest_loops.max(deepest_loops);
        self.most_loops = self.most_loops.max(loops);
        self.most_branch_points = self.most_branch_points.max(branch_points);
        self.most_block_values = self.most_block_values.max(block_values);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape_of_module(body: &str) -> CodeShape {
        let code = wat::parse_str(format!("(component (core module {body}))")).unwrap();
        CodeShape::of(&code).unwrap()
    }

    #[test]
    fn work_is_the_values_to_track_times_the_places_to_track_them() {
        let shape = shape_of_module(
            r#"(func (param i32) (result i32) (local i32 i32)
                 (block (loop
                     (br_if 0 (local.get 0))
                     (br_if 1 (local.get 1))))
                 local.get 0)"#,
        );
        // 3 locals, 1 value on the stack and 1 result, at 4 branch points and the start.
        assert_eq!(shape.work, 25 * 25);
        assert_eq!(shape.most_branch_points, 4);
    }

    #[test]
    fn work_is_counted_for_values_on_the_stack() {
        let shape = shape_of_module(
            r#"(func i32.const 0 i32.const 0 i32.const 0 drop drop drop
                 (block) (block))"#,
        );
        assert_eq!(shape.work, 9 * 9);
    }

    #[test]
    fn work_adds_up_across_functions_and_modules() {
        let code = wat::parse_str(
            r#"(component
                 (core module (func (param i32) (block)) (func (param i32 i32) (block) (block)))
                 (core module (func (result i32) (block) (block) (block) i32.const 0)))"#,
        )
        .unwrap();
        let shape = CodeShape::of(&code).unwrap();
        assert_eq!(shape.functions, 3);
        assert_eq!(shape.modules, 2);
        // 1 value times 2, 2 times 3, and 1 stack value plus 1 result times 4.
        assert_eq!(shape.work, 2 * 2 + 6 * 6 + 8 * 8);
    }

    #[test]
    fn each_kind_of_branch_point_is_counted() {
        let shape = shape_of_module(
            r#"(func (param i32)
                 (block (br_table 0 0 0 (local.get 0)))
                 (if (local.get 0) (then) (else)))"#,
        );
        // The block, a table of 2 targets and a default, the if and the else.
        assert_eq!(shape.most_branch_points, 1 + 3 + 1 + 1);
    }

    #[test]
    fn nested_loops_are_told_apart_from_loops_in_a_row() {
        let in_a_row = shape_of_module("(func (loop) (loop) (loop))");
        assert_eq!((in_a_row.most_loops, in_a_row.deepest_loops), (3, 1));

        let nested = shape_of_module("(func (loop (loop (loop))))");
        assert_eq!((nested.most_loops, nested.deepest_loops), (3, 3));

        // Blocks between them don't count, and don't confuse the count.
        let mixed = shape_of_module("(func (loop (block (loop)) (block (block))) (loop))");
        assert_eq!((mixed.most_loops, mixed.deepest_loops), (3, 2));
        let blocks = shape_of_module("(func (block (block (block))))");
        assert_eq!((blocks.most_loops, blocks.deepest_loops), (0, 0));
    }

    #[test]
    fn the_counts_for_loops_and_blocks_are_per_function() {
        let shape = shape_of_module(
            r#"(func (loop) (loop))
               (func (loop))
               (func (block))
               (func (block (result i32) i32.const 0) drop)
               (func (block (result i32) i32.const 0) drop)"#,
        );
        assert_eq!(shape.most_loops, 2);
        assert_eq!(shape.deepest_loops, 1);
        assert_eq!(shape.most_branch_points, 2);
        assert_eq!(shape.most_block_values, 1);
    }

    #[test]
    fn values_passed_through_blocks_are_counted() {
        let shape = shape_of_module(
            r#"(type $t (func (param i32 i32) (result i32 i32 i32)))
               (func
                 (block (result i32) i32.const 0)
                 drop
                 i32.const 0
                 i32.const 0
                 (block (type $t) unreachable)
                 unreachable)
               (func (block))"#,
        );
        assert_eq!(shape.most_block_values, 1 + 5);
    }

    #[test]
    fn values_in_signatures_are_counted() {
        let shape = shape_of_module(
            r#"(type (func (param i32 i32) (result i32)))
               (type (func (param i64)))
               (func (type 0) unreachable)
               (func (type 0) unreachable)
               (func (type 1))"#,
        );
        // 3 + 1 for the types, and 3 + 3 + 1 for the functions that use them.
        assert_eq!(shape.signature_values, 4 + 7);
    }

    #[test]
    fn the_size_of_declarations_is_counted() {
        let small = shape_of_module(r#"(func (export "a"))"#);
        let large = shape_of_module(r#"(func (export "aaaaaaaaaa"))"#);
        assert_eq!(large.declaration_bytes, small.declaration_bytes + 9);

        // Neither code nor data count, because they have limits of their own.
        let without = shape_of_module("(memory 1)");
        let with = shape_of_module(r#"(memory 1) (data (i32.const 0) "aaaaaaaaaa")"#);
        assert_eq!(with.declaration_bytes, without.declaration_bytes);
        let with = shape_of_module("(memory 1) (func nop nop nop nop nop nop nop nop)");
        // Only the type and the list of functions.
        assert!(with.declaration_bytes < without.declaration_bytes + 16);
    }

    #[test]
    fn the_largest_function_is_found() {
        let shape = shape_of_module("(func) (func nop nop nop) (func nop)");
        // A byte for the count of locals, each instruction, and the end.
        assert_eq!(shape.largest_function, 5);
        // Those, and a byte for each function's size.
        assert_eq!(shape.code_bytes, 3 + 2 + 5 + 3);
    }

    #[test]
    fn what_a_module_declares_is_counted() {
        let shape = shape_of_module(
            r#"(import "a" "b" (func))
               (import "a" "c" (global i32))
               (memory 1) (memory 2)
               (table 7 funcref) (table 5 funcref)
               (global i32 (i32.const 0))
               (export "m" (memory 0))
               (elem declare func 0)
               (data "x")
               (tag)"#,
        );
        assert_eq!(shape.module_imports, 2);
        assert_eq!(shape.memories, 2);
        assert_eq!(shape.table_entries, 12);
        // 2 imports, a global, an export, an element segment, a data segment, a tag and a type.
        assert_eq!(shape.declarations, 8);
    }

    #[test]
    fn what_a_component_holds_is_counted() {
        let code = wat::parse_str(
            r#"(component
                 (core module $a (global (export "e") i32 (i32.const 0)))
                 (core instance (instantiate $a))
                 (core instance (instantiate $a))
                 (component (component))
                 (type $f (func))
                 (import "f" (func $f (type $f)))
                 (core func (canon lower (func $f)))
                 (core func (canon lower (func $f)))
                 (core type (func))
                 (instance $i)
                 (alias core export 0 "e" (core global))
                 (export "g" (func $f)))"#,
        )
        .unwrap();
        let shape = CodeShape::of(&code).unwrap();
        assert_eq!(shape.nested_components, 2);
        assert_eq!(shape.modules, 1);
        assert_eq!(shape.module_instances, 2);
        assert_eq!(shape.canonical_functions, 2);
        // 2 instances of modules, a type, an import, a core type, an instance of a component, an
        // alias and an export, and the module's own global and export.
        assert_eq!(shape.declarations, 2 + 1 + 1 + 1 + 1 + 1 + 1 + 2);
    }

    #[test]
    fn invalid_code_is_an_error() {
        let code = wat::parse_str("(component (core module (func)))").unwrap();
        let mut broken = code.clone();
        broken.truncate(code.len() - 1);
        assert!(CodeShape::of(&broken).is_err());
        assert!(CodeShape::of(&[0, 1, 2, 3]).is_err());
        assert!(CodeShape::of(&[]).is_err());
    }

    #[test]
    fn invalid_function_bodies_are_an_error() {
        // Fine for the wat parser, but this tries to use a value that isn't there.
        let code = wat::parse_str("(component (core module (func i32.add drop)))").unwrap();
        assert!(CodeShape::of(&code).is_err());
    }

    #[test]
    fn reading_stops_as_soon_as_it_is_asked_to() {
        let code = wat::parse_str(
            r#"(component
                 (core module (func nop))
                 (core module (func nop))
                 (core module (func nop)))"#,
        )
        .unwrap();
        let shape = CodeShape::read(&code, |shape| shape.modules < 2).unwrap();
        assert_eq!(shape.modules, 2);
        assert_eq!(shape.functions, 1);
    }

    #[test]
    fn nothing_after_the_point_of_stopping_is_looked_at() {
        let code = wat::parse_str("(component (core module (func)))").unwrap();
        let mut broken = code.clone();
        broken.truncate(code.len() - 1);
        let shape = CodeShape::read(&broken, |_| false);
        assert!(shape.is_ok(), "{shape:?}");
    }
}

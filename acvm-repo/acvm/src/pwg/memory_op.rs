use std::collections::HashMap;

use acir::{
    AcirField,
    circuit::opcodes::MemOp,
    native_types::{Expression, Witness, WitnessMap},
};

use super::{ErrorLocation, OpcodeResolutionError};
use super::{
    arithmetic::ExpressionSolver, get_value, insert_value, is_predicate_false, witness_to_value,
};

type MemoryIndex = u32;

/// Maintains the state for solving [`MemoryInit`][`acir::circuit::Opcode::MemoryInit`] and [`MemoryOp`][`acir::circuit::Opcode::MemoryOp`] opcodes.
#[derive(Default)]
pub(crate) struct MemoryOpSolver<F> {
    /// Known values of the memory block, based on the index
    /// This map evolves as we process the opcodes
    pub(super) block_value: HashMap<MemoryIndex, F>,
    /// Length of the block, i.e the number of elements stored into the memory block.
    pub(super) block_len: u32,
}

impl<F: AcirField> MemoryOpSolver<F> {
    /// Convert a field element into a memory index
    /// Only 32 bits values are valid memory indices
    fn index_from_field(&self, index: F) -> Result<MemoryIndex, OpcodeResolutionError<F>> {
        if index.num_bits() <= 32 {
            let memory_index = index.try_to_u64().unwrap() as MemoryIndex;
            Ok(memory_index)
        } else {
            Err(OpcodeResolutionError::IndexOutOfBounds {
                opcode_location: ErrorLocation::Unresolved,
                index,
                array_size: self.block_len,
            })
        }
    }

    /// Update the 'block_value' map with the provided index/value
    /// Returns an 'IndexOutOfBounds' error if the index is outside the block range.
    fn write_memory_index(
        &mut self,
        index: MemoryIndex,
        value: F,
    ) -> Result<(), OpcodeResolutionError<F>> {
        if index >= self.block_len {
            return Err(OpcodeResolutionError::IndexOutOfBounds {
                opcode_location: ErrorLocation::Unresolved,
                index: F::from(index as u128),
                array_size: self.block_len,
            });
        }
        self.block_value.insert(index, value);
        Ok(())
    }

    /// Returns the value stored in the 'block_value' map for the provided index
    /// Returns an 'IndexOutOfBounds' error if the index is not in the map.
    fn read_memory_index(&self, index: MemoryIndex) -> Result<F, OpcodeResolutionError<F>> {
        self.block_value.get(&index).copied().ok_or(OpcodeResolutionError::IndexOutOfBounds {
            opcode_location: ErrorLocation::Unresolved,
            index: F::from(index as u128),
            array_size: self.block_len,
        })
    }

    /// Set the block_value from a MemoryInit opcode
    pub(crate) fn init(
        &mut self,
        init: &[Witness],
        initial_witness: &WitnessMap<F>,
    ) -> Result<(), OpcodeResolutionError<F>> {
        self.block_len = init.len() as u32;
        for (memory_index, witness) in init.iter().enumerate() {
            self.write_memory_index(
                memory_index as MemoryIndex,
                *witness_to_value(initial_witness, *witness)?,
            )?;
        }
        Ok(())
    }

    /// Update the 'block_values' by processing the provided Memory opcode
    /// The opcode 'op' contains the index and value of the operation and the type
    /// of the operation.
    /// They are all stored as an [Expression]
    /// The type of 'operation' is '0' for a read and '1' for a write. It must be a constant
    /// expression.
    /// Index is not required to be constant but it must reduce to a known value
    /// for processing the opcode. This is done by doing the (partial) evaluation of its expression,
    /// using the provided witness map.
    ///
    /// READ: read the block at index op.index and update op.value with the read value
    /// - 'op.value' must reduce to a witness (after the evaluation of its expression)
    /// - the value is updated in the provided witness map, for the 'op.value' witness
    ///
    /// WRITE: update the block at index 'op.index' with 'op.value'
    /// - 'op.value' must reduce to a known value
    ///
    /// If a requirement is not met, it returns an error.
    pub(crate) fn solve_memory_op(
        &mut self,
        op: &MemOp<F>,
        initial_witness: &mut WitnessMap<F>,
        predicate: &Option<Expression<F>>,
        pedantic_solving: bool,
    ) -> Result<(), OpcodeResolutionError<F>> {
        let operation = get_value(&op.operation, initial_witness)?;

        // Find the memory index associated with this memory operation.
        let index = get_value(&op.index, initial_witness)?;
        let memory_index = self.index_from_field(index)?;

        // Calculate the value associated with this memory operation.
        //
        // In read operations, this corresponds to the witness index at which the value from memory will be written.
        // In write operations, this corresponds to the expression which will be written to memory.
        let value = ExpressionSolver::evaluate(&op.value, initial_witness);

        // `operation == 0` implies a read operation. (`operation == 1` implies write operation).
        let is_read_operation = operation.is_zero();

        // Fetch whether or not the predicate is false (e.g. equal to zero)
        let opcode_location = ErrorLocation::Unresolved;
        let skip_operation =
            is_predicate_false(initial_witness, predicate, pedantic_solving, &opcode_location)?;

        if is_read_operation {
            // `value_read = arr[memory_index]`
            //
            // This is the value that we want to read into; i.e. copy from the memory block
            // into this value.
            let value_read_witness = value.to_witness().expect(
                "Memory must be read into a specified witness index, encountered an Expression",
            );

            // A zero predicate indicates that we should skip the read operation
            // and zero out the operation's output.
            let value_in_array =
                if skip_operation { F::zero() } else { self.read_memory_index(memory_index)? };
            insert_value(&value_read_witness, value_in_array, initial_witness)
        } else {
            // `arr[memory_index] = value_write`
            //
            // This is the value that we want to write into; i.e. copy from `value_write`
            // into the memory block.
            let value_write = value;

            // A zero predicate indicates that we should skip the write operation.
            if skip_operation {
                // We only want to write to already initialized memory.
                // Do nothing if the predicate is zero.
                Ok(())
            } else {
                let value_to_write = get_value(&value_write, initial_witness)?;
                self.write_memory_index(memory_index, value_to_write)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use acir::{
        AcirField, FieldElement,
        circuit::opcodes::MemOp,
        native_types::{Expression, Witness, WitnessMap},
    };

    use super::MemoryOpSolver;

    // use pedantic_solving for tests
    const PEDANTIC_SOLVING: bool = true;

    #[test]
    fn test_solver() {
        let mut initial_witness = WitnessMap::from(BTreeMap::from_iter([
            (Witness(1), FieldElement::from(1u128)),
            (Witness(2), FieldElement::from(1u128)),
            (Witness(3), FieldElement::from(2u128)),
        ]));

        let init = vec![Witness(1), Witness(2)];

        let trace = vec![
            MemOp::write_to_mem_index(FieldElement::from(1u128).into(), Witness(3).into()),
            MemOp::read_at_mem_index(FieldElement::one().into(), Witness(4)),
        ];

        let mut block_solver = MemoryOpSolver::default();
        block_solver.init(&init, &initial_witness).unwrap();

        for op in trace {
            block_solver
                .solve_memory_op(&op, &mut initial_witness, &None, PEDANTIC_SOLVING)
                .unwrap();
        }

        assert_eq!(initial_witness[&Witness(4)], FieldElement::from(2u128));
    }

    #[test]
    fn test_index_out_of_bounds() {
        let mut initial_witness = WitnessMap::from(BTreeMap::from_iter([
            (Witness(1), FieldElement::from(1u128)),
            (Witness(2), FieldElement::from(1u128)),
            (Witness(3), FieldElement::from(2u128)),
        ]));

        let init = vec![Witness(1), Witness(2)];

        let invalid_trace = vec![
            MemOp::write_to_mem_index(FieldElement::from(1u128).into(), Witness(3).into()),
            MemOp::read_at_mem_index(FieldElement::from(2u128).into(), Witness(4)),
        ];
        let mut block_solver = MemoryOpSolver::default();
        block_solver.init(&init, &initial_witness).unwrap();
        let mut err = None;
        for op in invalid_trace {
            if err.is_none() {
                err = block_solver
                    .solve_memory_op(&op, &mut initial_witness, &None, PEDANTIC_SOLVING)
                    .err();
            }
        }

        assert!(matches!(
            err,
            Some(crate::pwg::OpcodeResolutionError::IndexOutOfBounds {
                opcode_location: _,
                index,
                array_size: 2
            }) if index == FieldElement::from(2u128)
        ));
    }

    #[test]
    fn test_predicate_on_read() {
        let mut initial_witness = WitnessMap::from(BTreeMap::from_iter([
            (Witness(1), FieldElement::from(1u128)),
            (Witness(2), FieldElement::from(1u128)),
            (Witness(3), FieldElement::from(2u128)),
        ]));

        let init = vec![Witness(1), Witness(2)];

        let invalid_trace = vec![
            MemOp::write_to_mem_index(FieldElement::from(1u128).into(), Witness(3).into()),
            MemOp::read_at_mem_index(FieldElement::from(2u128).into(), Witness(4)),
        ];
        let mut block_solver = MemoryOpSolver::default();
        block_solver.init(&init, &initial_witness).unwrap();
        let mut err = None;
        for op in invalid_trace {
            if err.is_none() {
                err = block_solver
                    .solve_memory_op(
                        &op,
                        &mut initial_witness,
                        &Some(Expression::zero()),
                        PEDANTIC_SOLVING,
                    )
                    .err();
            }
        }

        // Should have no index out of bounds error where predicate is zero
        assert_eq!(err, None);
        // The result of a read under a zero predicate should be zero
        assert_eq!(initial_witness[&Witness(4)], FieldElement::from(0u128));
    }

    #[test]
    fn test_predicate_on_write() {
        let mut initial_witness = WitnessMap::from(BTreeMap::from_iter([
            (Witness(1), FieldElement::from(1u128)),
            (Witness(2), FieldElement::from(1u128)),
            (Witness(3), FieldElement::from(2u128)),
        ]));

        let init = vec![Witness(1), Witness(2)];

        let invalid_trace = vec![
            MemOp::write_to_mem_index(FieldElement::from(2u128).into(), Witness(3).into()),
            MemOp::read_at_mem_index(FieldElement::from(0u128).into(), Witness(4)),
            MemOp::read_at_mem_index(FieldElement::from(1u128).into(), Witness(5)),
        ];
        let mut block_solver = MemoryOpSolver::default();
        block_solver.init(&init, &initial_witness).unwrap();
        let mut err = None;
        for op in invalid_trace {
            if err.is_none() {
                err = block_solver
                    .solve_memory_op(
                        &op,
                        &mut initial_witness,
                        &Some(Expression::zero()),
                        PEDANTIC_SOLVING,
                    )
                    .err();
            }
        }

        // Should have no index out of bounds error where predicate is zero
        assert_eq!(err, None);
        // The memory under a zero predicate should be zeroed out
        assert_eq!(initial_witness[&Witness(4)], FieldElement::from(0u128));
        assert_eq!(initial_witness[&Witness(5)], FieldElement::from(0u128));
    }
}

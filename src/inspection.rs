use crate::{
    InspectionCompletion, InspectionExhaustion, InspectionLimit, InspectionLimits, InspectionUsage,
    VariableUnavailableReason,
};

pub const MAX_INSPECTION_LIMITS: InspectionLimits = InspectionLimits {
    variables: 4_096,
    value_nodes: 4_096,
    aggregate_depth: 64,
    memory_reads: 1_024,
    memory_bytes: 1024 * 1024,
    expression_work: 10_240_000,
    output_bytes: 1024 * 1024,
};

#[derive(Debug)]
pub struct InspectionBudget {
    limits: InspectionLimits,
    usage: InspectionUsage,
    exhaustion: Option<InspectionExhaustion>,
}

impl Default for InspectionBudget {
    fn default() -> Self {
        Self::new(InspectionLimits::default())
    }
}

impl InspectionBudget {
    pub const fn new(limits: InspectionLimits) -> Self {
        Self {
            limits,
            usage: InspectionUsage {
                variables: 0,
                value_nodes: 0,
                aggregate_depth: 0,
                memory_reads: 0,
                memory_bytes: 0,
                expression_work: 0,
                output_bytes: 0,
            },
            exhaustion: None,
        }
    }

    pub const fn usage(&self) -> InspectionUsage {
        self.usage
    }

    pub const fn completion(&self) -> InspectionCompletion {
        match self.exhaustion {
            Some(exhaustion) => InspectionCompletion::Truncated(exhaustion),
            None => InspectionCompletion::Complete,
        }
    }

    pub const fn exhaustion(&self) -> Option<InspectionExhaustion> {
        self.exhaustion
    }

    pub const fn remaining_value_nodes(&self) -> u64 {
        self.limits
            .value_nodes
            .saturating_sub(self.usage.value_nodes)
    }

    pub const fn remaining_memory_reads(&self) -> u64 {
        self.limits
            .memory_reads
            .saturating_sub(self.usage.memory_reads)
    }

    pub const fn remaining_memory_bytes(&self) -> u64 {
        self.limits
            .memory_bytes
            .saturating_sub(self.usage.memory_bytes)
    }

    pub fn consume_variable_value(&mut self) -> Result<(), InspectionExhaustion> {
        self.can_consume(
            InspectionLimit::Variables,
            self.limits.variables,
            self.usage.variables,
            1,
        )?;
        self.can_consume(
            InspectionLimit::ValueNodes,
            self.limits.value_nodes,
            self.usage.value_nodes,
            1,
        )?;
        self.usage.variables += 1;
        self.usage.value_nodes += 1;
        Ok(())
    }

    pub fn consume_value_nodes(&mut self, amount: u64) -> Result<(), InspectionExhaustion> {
        Self::consume(
            &mut self.exhaustion,
            InspectionLimit::ValueNodes,
            self.limits.value_nodes,
            &mut self.usage.value_nodes,
            amount,
        )
    }

    pub fn observe_aggregate_depth(&mut self, depth: u64) -> Result<(), InspectionExhaustion> {
        if let Some(exhaustion) = self.exhaustion {
            return Err(exhaustion);
        }
        if depth > self.limits.aggregate_depth {
            let exhaustion = InspectionExhaustion {
                resource: InspectionLimit::AggregateDepth,
                limit: self.limits.aggregate_depth,
                used: self.usage.aggregate_depth,
                requested: depth,
            };
            self.exhaustion = Some(exhaustion);
            return Err(exhaustion);
        }
        self.usage.aggregate_depth = self.usage.aggregate_depth.max(depth);
        Ok(())
    }

    pub fn consume_memory(&mut self, bytes: usize) -> Result<(), InspectionExhaustion> {
        let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
        self.can_consume(
            InspectionLimit::MemoryReads,
            self.limits.memory_reads,
            self.usage.memory_reads,
            1,
        )?;
        self.can_consume(
            InspectionLimit::MemoryBytes,
            self.limits.memory_bytes,
            self.usage.memory_bytes,
            bytes,
        )?;
        self.usage.memory_reads += 1;
        self.usage.memory_bytes += bytes;
        Ok(())
    }

    pub fn consume_expression_work(&mut self, amount: u64) -> Result<(), InspectionExhaustion> {
        Self::consume(
            &mut self.exhaustion,
            InspectionLimit::ExpressionWork,
            self.limits.expression_work,
            &mut self.usage.expression_work,
            amount,
        )
    }

    fn can_consume(
        &mut self,
        resource: InspectionLimit,
        limit: u64,
        used: u64,
        requested: u64,
    ) -> Result<(), InspectionExhaustion> {
        if let Some(exhaustion) = self.exhaustion {
            return Err(exhaustion);
        }
        if used.checked_add(requested).is_none_or(|next| next > limit) {
            let exhaustion = InspectionExhaustion {
                resource,
                limit,
                used,
                requested,
            };
            self.exhaustion = Some(exhaustion);
            return Err(exhaustion);
        }
        Ok(())
    }

    fn consume(
        exhaustion: &mut Option<InspectionExhaustion>,
        resource: InspectionLimit,
        limit: u64,
        used: &mut u64,
        requested: u64,
    ) -> Result<(), InspectionExhaustion> {
        if let Some(exhaustion) = *exhaustion {
            return Err(exhaustion);
        }
        let Some(next) = used.checked_add(requested).filter(|next| *next <= limit) else {
            let value = InspectionExhaustion {
                resource,
                limit,
                used: *used,
                requested,
            };
            *exhaustion = Some(value);
            return Err(value);
        };
        *used = next;
        Ok(())
    }
}

impl From<InspectionExhaustion> for VariableUnavailableReason {
    fn from(exhaustion: InspectionExhaustion) -> Self {
        Self::InspectionLimit(exhaustion)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> InspectionLimits {
        InspectionLimits {
            variables: 2,
            value_nodes: 2,
            aggregate_depth: 2,
            memory_reads: 2,
            memory_bytes: 4,
            expression_work: 2,
            output_bytes: 64,
        }
    }

    #[test]
    fn exact_budget_boundaries_succeed_and_one_more_is_typed() {
        let mut budget = InspectionBudget::new(limits());
        budget
            .consume_variable_value()
            .expect("first variable fits");
        budget
            .consume_variable_value()
            .expect("exact variable limit");
        let exhaustion = budget.consume_variable_value().expect_err("one over limit");
        assert_eq!(
            exhaustion,
            InspectionExhaustion {
                resource: InspectionLimit::Variables,
                limit: 2,
                used: 2,
                requested: 1,
            }
        );
        assert_eq!(
            budget.completion(),
            InspectionCompletion::Truncated(exhaustion)
        );
    }

    #[test]
    fn memory_reservations_are_atomic_and_precede_later_work() {
        let mut budget = InspectionBudget::new(limits());
        budget.consume_memory(4).expect("exact byte limit");
        assert_eq!(
            budget
                .consume_memory(1)
                .expect_err("bytes exhausted")
                .resource,
            InspectionLimit::MemoryBytes
        );
        assert_eq!(budget.usage().memory_reads, 1);
        assert_eq!(budget.usage().memory_bytes, 4);
        assert!(budget.consume_value_nodes(1).is_err());
        assert_eq!(budget.usage().value_nodes, 0);
    }

    #[test]
    fn variable_value_reservations_are_atomic() {
        let mut limits = limits();
        limits.value_nodes = 1;
        let mut budget = InspectionBudget::new(limits);

        budget
            .consume_variable_value()
            .expect("first variable and value node fit");
        let exhaustion = budget
            .consume_variable_value()
            .expect_err("second value node exceeds its limit");

        assert_eq!(exhaustion.resource, InspectionLimit::ValueNodes);
        assert_eq!(budget.usage().variables, 1);
        assert_eq!(budget.usage().value_nodes, 1);
    }
}

use crate::{
    InspectionCompletion, InspectionExhaustion, InspectionLimit, InspectionLimits, InspectionUsage,
    VariableUnavailableReason,
};

/// The largest limits a client may request for one operation.
pub const MAX_INSPECTION_LIMITS: InspectionLimits = InspectionLimits {
    variables: 4_096,
    value_nodes: 4_096,
    aggregate_depth: 64,
    memory_reads: 1_024,
    memory_bytes: 1024 * 1024,
    expression_work: 10_240_000,
};

/// Tracks the resources one inspection operation has used against its limits.
///
/// Exhaustion is sticky: once any reservation fails, every later one fails
/// with the same exhaustion, so a truncated result reports its first cause.
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
        self.reserve(&[
            (InspectionLimit::Variables, 1),
            (InspectionLimit::ValueNodes, 1),
        ])
    }

    pub fn consume_value_nodes(&mut self, amount: u64) -> Result<(), InspectionExhaustion> {
        self.reserve(&[(InspectionLimit::ValueNodes, amount)])
    }

    pub fn consume_memory(&mut self, bytes: usize) -> Result<(), InspectionExhaustion> {
        let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
        self.reserve(&[
            (InspectionLimit::MemoryReads, 1),
            (InspectionLimit::MemoryBytes, bytes),
        ])
    }

    pub fn consume_expression_work(&mut self, amount: u64) -> Result<(), InspectionExhaustion> {
        self.reserve(&[(InspectionLimit::ExpressionWork, amount)])
    }

    /// Reserves every request or, if any would exceed its limit, none.
    fn reserve(&mut self, requests: &[(InspectionLimit, u64)]) -> Result<(), InspectionExhaustion> {
        if let Some(exhaustion) = self.exhaustion {
            return Err(exhaustion);
        }
        for &(resource, requested) in requests {
            let (limit, used) = self.resource(resource);
            if used.checked_add(requested).is_none_or(|next| next > limit) {
                return Err(self.exhaust(resource, requested));
            }
        }
        for &(resource, requested) in requests {
            *self.used_mut(resource) += requested;
        }
        Ok(())
    }

    const fn exhaust(&mut self, resource: InspectionLimit, requested: u64) -> InspectionExhaustion {
        let (limit, used) = self.resource(resource);
        let exhaustion = InspectionExhaustion {
            resource,
            limit,
            used,
            requested,
        };
        self.exhaustion = Some(exhaustion);
        exhaustion
    }

    /// Returns a resource's limit and current usage.
    const fn resource(&self, resource: InspectionLimit) -> (u64, u64) {
        let (limits, usage) = (&self.limits, &self.usage);
        match resource {
            InspectionLimit::Variables => (limits.variables, usage.variables),
            InspectionLimit::ValueNodes => (limits.value_nodes, usage.value_nodes),
            InspectionLimit::AggregateDepth => (limits.aggregate_depth, usage.aggregate_depth),
            InspectionLimit::MemoryReads => (limits.memory_reads, usage.memory_reads),
            InspectionLimit::MemoryBytes => (limits.memory_bytes, usage.memory_bytes),
            InspectionLimit::ExpressionWork => (limits.expression_work, usage.expression_work),
        }
    }

    const fn used_mut(&mut self, resource: InspectionLimit) -> &mut u64 {
        let usage = &mut self.usage;
        match resource {
            InspectionLimit::Variables => &mut usage.variables,
            InspectionLimit::ValueNodes => &mut usage.value_nodes,
            InspectionLimit::AggregateDepth => &mut usage.aggregate_depth,
            InspectionLimit::MemoryReads => &mut usage.memory_reads,
            InspectionLimit::MemoryBytes => &mut usage.memory_bytes,
            InspectionLimit::ExpressionWork => &mut usage.expression_work,
        }
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

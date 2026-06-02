use alloy::primitives::Address;
use anyhow::Context;

use crate::state::{EcosystemInitOutput, State, StepKey};

/// Resolved ecosystem addresses derived from `state.json` in one step.
///
/// Eliminates repeated `state.get_output(StepKey::EcosystemInit).context(...)` +
/// field access patterns across `apply` and `bootstrap` commands.
#[derive(Debug, Clone)]
pub struct ResolvedEcosystem {
    pub bridgehub: Address,
    pub ctm_proxy: Address,
    pub governance: Address,
    pub rollup_l1_da_validator: Address,
    pub no_da_l1_validator: Address,
    pub avail_l1_da_validator: Address,
    pub blobs_zksync_os_l1_da_validator: Option<Address>,
    pub bytecodes_supplier: Option<Address>,
}

impl ResolvedEcosystem {
    pub fn from_state(state: &State) -> anyhow::Result<Self> {
        let eco: EcosystemInitOutput = state
            .get_output(StepKey::EcosystemInit)
            .context("ecosystem.init not found in state — run `bootstrap` first")?;
        Ok(Self {
            bridgehub: eco.bridgehub_proxy,
            ctm_proxy: eco.ctm_proxy,
            governance: eco.governance,
            rollup_l1_da_validator: eco.rollup_l1_da_validator,
            no_da_l1_validator: eco.no_da_l1_validator,
            avail_l1_da_validator: eco.avail_l1_da_validator,
            blobs_zksync_os_l1_da_validator: eco.blobs_zksync_os_l1_da_validator,
            bytecodes_supplier: eco.bytecodes_supplier,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::State;
    use alloy::primitives::Address;

    fn make_eco(bridgehub_byte: u8) -> EcosystemInitOutput {
        EcosystemInitOutput {
            bridgehub_proxy: Address::repeat_byte(bridgehub_byte),
            ctm_proxy: Address::repeat_byte(2),
            bytecodes_supplier: None,
            rollup_l1_da_validator: Address::repeat_byte(3),
            no_da_l1_validator: Address::repeat_byte(4),
            avail_l1_da_validator: Address::repeat_byte(5),
            blobs_zksync_os_l1_da_validator: None,
            governance: Address::repeat_byte(6),
        }
    }

    #[test]
    fn missing_key_errors() {
        let state = State::new();
        let result = ResolvedEcosystem::from_state(&state);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("ecosystem.init"));
    }

    #[test]
    fn happy_path() {
        let mut state = State::new();
        state
            .mark_done(&StepKey::EcosystemInit, &make_eco(1))
            .unwrap();
        let resolved = ResolvedEcosystem::from_state(&state).unwrap();
        assert_eq!(resolved.bridgehub, Address::repeat_byte(1));
        assert_eq!(resolved.ctm_proxy, Address::repeat_byte(2));
    }
}

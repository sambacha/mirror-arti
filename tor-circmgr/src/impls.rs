//! Implement traits from [`crate::mgr`] for the circuit types we use.

use crate::mgr::{self};
use crate::path::OwnedPath;
use crate::usage::{SupportedCircUsage, TargetCircUsage};
use crate::{DirInfo, Result};
use async_trait::async_trait;
use rand::{rngs::StdRng, SeedableRng};
use std::convert::TryInto;
use std::sync::Arc;
use tor_proto::circuit::{CircParameters, ClientCirc};
use tor_rtcompat::Runtime;

impl mgr::AbstractCirc for tor_proto::circuit::ClientCirc {
    type Id = tor_proto::circuit::UniqId;
    fn id(&self) -> Self::Id {
        self.unique_id()
    }
    fn usable(&self) -> bool {
        !self.is_closing()
    }
}

/// The information generated by circuit planning, and used to build a
/// circuit.
pub(crate) struct Plan {
    /// The supported usage that the circuit will have when complete
    final_spec: SupportedCircUsage,
    /// An owned copy of the path to build.
    // TODO: it would be nice if this weren't owned.
    path: OwnedPath,
    /// The protocol parameters to use when constructing the circuit.
    params: CircParameters,
}

#[async_trait]
impl<R: Runtime> crate::mgr::AbstractCircBuilder for crate::build::CircuitBuilder<R> {
    type Circ = ClientCirc;
    type Spec = SupportedCircUsage;
    type Plan = Plan;

    fn plan_circuit(
        &self,
        usage: &TargetCircUsage,
        dir: DirInfo<'_>,
    ) -> Result<(Plan, SupportedCircUsage)> {
        let mut rng = rand::thread_rng();
        let (path, final_spec) = usage.build_path(&mut rng, dir)?;

        let plan = Plan {
            final_spec: final_spec.clone(),
            path: (&path).try_into()?,
            params: dir.circ_params(),
        };

        Ok((plan, final_spec))
    }

    async fn build_circuit(&self, plan: Plan) -> Result<(SupportedCircUsage, Arc<ClientCirc>)> {
        let Plan {
            final_spec,
            path,
            params,
        } = plan;
        let rng = StdRng::from_rng(rand::thread_rng()).expect("couldn't construct temporary rng");

        let circuit = self.build_owned(path, &params, rng).await?;
        Ok((final_spec, circuit))
    }

    fn launch_parallelism(&self, spec: &TargetCircUsage) -> usize {
        match spec {
            TargetCircUsage::Dir => 3,
            _ => 1,
        }
    }

    fn select_parallelism(&self, spec: &TargetCircUsage) -> usize {
        self.launch_parallelism(spec)
    }
}

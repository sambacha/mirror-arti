//! Functions to download or load directory objects, using the
//! state machines in the `states` module.

use std::ops::Deref;
use std::{
    collections::HashMap,
    sync::{Arc, Weak},
    time::{Duration, SystemTime},
};

use crate::{
    docid::{self, ClientRequest},
    state::WriteNetDir,
    upgrade_weak_ref, DirMgr, DirState, DocId, DocQuery, DocumentText, Error, Readiness, Result,
};

use futures::channel::oneshot;
use futures::FutureExt;
use futures::StreamExt;
use tor_checkable::TimeValidityError;
use tor_dirclient::DirResponse;
use tor_rtcompat::{Runtime, SleepProviderExt};
use tracing::{debug, info, trace, warn};

use crate::storage::Store;
#[cfg(test)]
use once_cell::sync::Lazy;
#[cfg(test)]
use std::sync::Mutex;
use tor_netdoc::doc::netstatus::ConsensusFlavor;

/// Load a set of documents from a `Store`, returning all documents found in the store.
/// Note that this may be less than the number of documents in `missing`.
fn load_documents_from_store(
    missing: &[DocId],
    store: &dyn Store,
) -> Result<HashMap<DocId, DocumentText>> {
    let mut loaded = HashMap::new();
    for query in docid::partition_by_type(missing.iter().copied()).values() {
        query.load_from_store_into(&mut loaded, store)?;
    }
    Ok(loaded)
}

/// Construct an appropriate ClientRequest to download a consensus
/// of the given flavor.
// FIXME(eta): remove pub
pub(crate) fn make_consensus_request(
    now: SystemTime,
    flavor: ConsensusFlavor,
    store: &dyn Store,
) -> Result<ClientRequest> {
    let mut request = tor_dirclient::request::ConsensusRequest::new(flavor);

    let default_cutoff = crate::default_consensus_cutoff(now)?;

    match store.latest_consensus_meta(flavor) {
        Ok(Some(meta)) => {
            let valid_after = meta.lifetime().valid_after();
            request.set_last_consensus_date(std::cmp::max(valid_after, default_cutoff));
            request.push_old_consensus_digest(*meta.sha3_256_of_signed());
        }
        latest => {
            if let Err(e) = latest {
                warn!("Error loading directory metadata: {}", e);
            }
            // If we don't have a consensus, then request one that's
            // "reasonably new".  That way, our clock is set far in the
            // future, we won't download stuff we can't use.
            request.set_last_consensus_date(default_cutoff);
        }
    }

    Ok(ClientRequest::Consensus(request))
}

/// Construct a set of `ClientRequest`s in order to fetch the documents in `docs`.
// FIXME(eta): remove pub
pub(crate) fn make_requests_for_documents<R: Runtime>(
    rt: &R,
    docs: &[DocId],
    store: &dyn Store,
) -> Result<Vec<ClientRequest>> {
    let mut res = Vec::new();
    for q in docid::partition_by_type(docs.iter().copied())
        .into_iter()
        .flat_map(|(_, x)| x.split_for_download().into_iter())
    {
        match q {
            DocQuery::LatestConsensus { flavor, .. } => {
                res.push(make_consensus_request(rt.wallclock(), flavor, store)?);
            }
            DocQuery::AuthCert(ids) => {
                res.push(ClientRequest::AuthCert(ids.into_iter().collect()));
            }
            DocQuery::Microdesc(ids) => {
                res.push(ClientRequest::Microdescs(ids.into_iter().collect()));
            }
            #[cfg(feature = "routerdesc")]
            DocQuery::RouterDesc(ids) => {
                res.push(ClientRequest::RouterDescs(ids.into_iter().collect()));
            }
        }
    }
    Ok(res)
}

/// Testing helper: if this is Some, then we return it in place of any
/// response to fetch_single.
///
/// Note that only one test uses this: otherwise there would be a race
/// condition. :p
#[cfg(test)]
static CANNED_RESPONSE: Lazy<Mutex<Option<String>>> = Lazy::new(|| Mutex::new(None));

/// Launch a single client request and get an associated response.
async fn fetch_single<R: Runtime>(
    dirmgr: Arc<DirMgr<R>>,
    request: ClientRequest,
) -> Result<(ClientRequest, DirResponse)> {
    #[cfg(test)]
    {
        let m = CANNED_RESPONSE.lock().expect("Poisoned mutex");
        if let Some(s) = m.as_ref() {
            return Ok((request, DirResponse::from_body(s)));
        }
    }
    let circmgr = dirmgr.circmgr()?;
    let cur_netdir = dirmgr.opt_netdir();
    let dirinfo = match cur_netdir {
        Some(ref netdir) => netdir.as_ref().into(),
        None => tor_circmgr::DirInfo::Nothing,
    };
    let outcome =
        tor_dirclient::get_resource(request.as_requestable(), dirinfo, &dirmgr.runtime, circmgr)
            .await;

    dirmgr.note_request_outcome(&outcome);

    let resource = outcome?;
    Ok((request, resource))
}

/// Launch a set of download requests for a set of missing objects in
/// `missing`, and return each request along with the response it received.
///
/// Don't launch more than `parallelism` requests at once.
async fn fetch_multiple<R: Runtime>(
    dirmgr: Arc<DirMgr<R>>,
    missing: &[DocId],
    parallelism: usize,
) -> Result<Vec<(ClientRequest, DirResponse)>> {
    let requests = {
        let store = dirmgr.store.lock().expect("store lock poisoned");
        make_requests_for_documents(&dirmgr.runtime, missing, store.deref())?
    };

    // TODO: instead of waiting for all the queries to finish, we
    // could stream the responses back or something.
    let responses: Vec<Result<(ClientRequest, DirResponse)>> = futures::stream::iter(requests)
        .map(|query| fetch_single(Arc::clone(&dirmgr), query))
        .buffer_unordered(parallelism)
        .collect()
        .await;

    let mut useful_responses = Vec::new();
    for r in responses {
        // TODO: on some error cases we might want to stop using this source.
        match r {
            Ok((request, response)) => {
                if response.status_code() == 200 {
                    useful_responses.push((request, response));
                } else {
                    trace!(
                        "cache declined request; reported status {:?}",
                        response.status_code()
                    );
                }
            }
            Err(e) => warn!("error while downloading: {:?}", e),
        }
    }

    Ok(useful_responses)
}

/// Try tp update `state` by loading cached information from `dirmgr`.
/// Return true if anything changed.
async fn load_once<R: Runtime>(
    dirmgr: &Arc<DirMgr<R>>,
    state: &mut Box<dyn DirState>,
) -> Result<bool> {
    let missing = state.missing_docs();
    let outcome = if missing.is_empty() {
        trace!("Found no missing documents; can't advance current state");
        Ok(false)
    } else {
        trace!(
            "Found {} missing documents; trying to load them",
            missing.len()
        );

        let documents = {
            let store = dirmgr.store.lock().expect("store lock poisoned");
            load_documents_from_store(&missing, store.deref())?
        };

        match state.add_from_cache(documents, dirmgr.store_if_rw()) {
            Err(Error::UntimelyObject(TimeValidityError::Expired(_))) => {
                // This is just an expired object from the cache; we don't need
                // to call that an error.  Treat it as if it were absent.
                Ok(false)
            }
            other => other,
        }
    };

    if matches!(outcome, Ok(true)) {
        dirmgr.update_status(state.bootstrap_status());
    }

    outcome
}

/// Try to load as much state as possible for a provided `state` from the
/// cache in `dirmgr`, advancing the state to the extent possible.
///
/// No downloads are performed; the provided state will not be reset.
pub(crate) async fn load<R: Runtime>(
    dirmgr: Arc<DirMgr<R>>,
    mut state: Box<dyn DirState>,
) -> Result<Box<dyn DirState>> {
    let mut safety_counter = 0_usize;
    loop {
        trace!(state=%state.describe(), "Loading from cache");
        let changed = load_once(&dirmgr, &mut state).await?;

        if state.can_advance() {
            state = state.advance()?;
            safety_counter = 0;
        } else {
            if !changed {
                break;
            }
            safety_counter += 1;
            assert!(
                safety_counter < 100,
                "Spent 100 iterations in the same state: this is a bug"
            );
        }
    }

    Ok(state)
}

/// Helper: Make a set of download attempts for the current directory state,
/// and on success feed their results into the state object.
///
/// This can launch one or more download requests, but will not launch more
/// than `parallelism` requests at a time.
///
/// Return true if the state reports that it changed.
async fn download_attempt<R: Runtime>(
    dirmgr: &Arc<DirMgr<R>>,
    state: &mut Box<dyn DirState>,
    parallelism: usize,
) -> Result<bool> {
    let mut changed = false;
    let missing = state.missing_docs();
    let fetched = fetch_multiple(Arc::clone(dirmgr), &missing, parallelism).await?;
    for (client_req, dir_response) in fetched {
        let source = dir_response.source().map(Clone::clone);
        let text = match String::from_utf8(dir_response.into_output())
            .map_err(Error::BadUtf8FromDirectory)
        {
            Ok(t) => t,
            Err(e) => {
                if let Some(source) = source {
                    dirmgr.note_cache_error(&source, &e);
                }
                continue;
            }
        };
        match dirmgr.expand_response_text(&client_req, text) {
            Ok(text) => {
                let outcome = state.add_from_download(&text, &client_req, Some(&dirmgr.store));
                match outcome {
                    Ok(b) => {
                        changed |= b;
                        if let Some(source) = source {
                            dirmgr.note_cache_success(&source);
                        }
                    }
                    Err(e) => {
                        warn!("error while adding directory info: {}", e);
                        if let Some(source) = source {
                            dirmgr.note_cache_error(&source, &e);
                        }
                    }
                }
            }
            Err(e) => {
                warn!("Error when expanding directory text: {}", e);
                if let Some(source) = source {
                    dirmgr.note_cache_error(&source, &e);
                }
            }
        }
    }

    if changed {
        dirmgr.update_status(state.bootstrap_status());
    }

    Ok(changed)
}

/// Download information into a DirState state machine until it is
/// ["complete"](Readiness::Complete), or until we hit a
/// non-recoverable error.
///
/// Use `dirmgr` to load from the cache or to launch downloads.
///
/// Keep resetting the state as needed.
///
/// The first time that the state becomes ["usable"](Readiness::Usable),
/// notify the sender in `on_usable`.
///
/// Return Err only on a non-recoverable error.  On an error that
/// merits another bootstrap attempt with the same state, return the
/// state and an Error object in an option.
pub(crate) async fn download<R: Runtime>(
    dirmgr: Weak<DirMgr<R>>,
    mut state: Box<dyn DirState>,
    on_usable: &mut Option<oneshot::Sender<()>>,
) -> Result<(Box<dyn DirState>, Option<Error>)> {
    let runtime = upgrade_weak_ref(&dirmgr)?.runtime.clone();

    'next_state: loop {
        let retry_config = state.dl_config()?;
        let parallelism = retry_config.parallelism();

        // In theory this could be inside the loop below maybe?  If we
        // want to drop the restriction that the missing() members of a
        // state must never grow, then we'll need to move it inside.
        let mut now = {
            let dirmgr = upgrade_weak_ref(&dirmgr)?;
            load_once(&dirmgr, &mut state).await?;
            dirmgr.now()
        };

        // Skip the downloads if we can...
        if state.can_advance() {
            state = state.advance()?;
            continue 'next_state;
        }
        if state.is_ready(Readiness::Complete) {
            return Ok((state, None));
        }

        let reset_time = no_more_than_a_week_from(runtime.wallclock(), state.reset_time());
        let mut reset_timeout_future = runtime.sleep_until_wallclock(reset_time).fuse();

        let mut retry = retry_config.schedule();
        let mut delay = None;

        // Make several attempts to fetch whatever we're missing,
        // until either we can advance, or we've got a complete
        // document, or we run out of tries, or we run out of time.
        'next_attempt: for attempt in retry_config.attempts() {
            // We wait at the start of this loop, on all attempts but the first.
            // This ensures that we always wait between attempts, but not after
            // the final attempt.
            let next_delay = retry.next_delay(&mut rand::thread_rng());
            if let Some(delay) = delay.replace(next_delay) {
                debug!("Waiting {:?} for next download attempt...", delay);
                futures::select_biased! {
                    _ = reset_timeout_future => {
                        info!("Download attempt timed out completely; resetting download state.");
                        state = state.reset()?;
                        continue 'next_state;
                    }
                    _ = FutureExt::fuse(runtime.sleep(delay)) => {}
                };
            }

            info!("{}: {}", attempt + 1, state.describe());
            let reset_time = no_more_than_a_week_from(now, state.reset_time());

            now = {
                let dirmgr = upgrade_weak_ref(&dirmgr)?;
                futures::select_biased! {
                    outcome = download_attempt(&dirmgr, &mut state, parallelism.into()).fuse() => {
                        match outcome {
                            Err(e) => {
                                warn!("Error while downloading: {}", e);
                                continue 'next_attempt;
                            }
                            Ok(changed) => {
                                changed
                            }
                        }
                    }
                    _ = runtime.sleep_until_wallclock(reset_time).fuse() => {
                        // We need to reset. This can happen if (for
                        // example) we're downloading the last few
                        // microdescriptors on a consensus that now
                        // we're ready to replace.
                        state = state.reset()?;
                        continue 'next_state;
                    },
                };
                dirmgr.now()
            };

            // Exit if there is nothing more to download.
            if state.is_ready(Readiness::Complete) {
                return Ok((state, None));
            }

            // Report usable-ness if appropriate.
            if on_usable.is_some() && state.is_ready(Readiness::Usable) {
                // Unwrap should be safe due to parent `.is_some()` check
                #[allow(clippy::unwrap_used)]
                let _ = on_usable.take().unwrap().send(());
            }

            if state.can_advance() {
                // We have enough info to advance to another state.
                state = state.advance()?;
                continue 'next_state;
            }
        }

        // We didn't advance the state, after all the retries.
        warn!(n_attempts=retry_config.n_attempts(),
              state=%state.describe(),
              "Unable to advance downloading state");
        return Ok((state, Some(Error::CantAdvanceState)));
    }
}

/// Helper: Clamp `v` so that it is no more than one week from `now`.
///
/// If `v` is absent, return the time that's one week from now.
///
/// We use this to determine a reset time when no reset time is
/// available, or when it is too far in the future.
fn no_more_than_a_week_from(now: SystemTime, v: Option<SystemTime>) -> SystemTime {
    let one_week_later = now + Duration::new(86400 * 7, 0);
    match v {
        Some(t) => std::cmp::min(t, one_week_later),
        None => one_week_later,
    }
}

#[cfg(test)]
mod test {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::storage::DynStore;
    use crate::test::new_mgr;
    use crate::DownloadSchedule;
    use std::sync::Mutex;
    use tor_netdoc::doc::microdesc::MdDigest;
    use tor_rtcompat::SleepProvider;

    #[test]
    fn week() {
        let now = SystemTime::now();
        let one_day = Duration::new(86400, 0);

        assert_eq!(no_more_than_a_week_from(now, None), now + one_day * 7);
        assert_eq!(
            no_more_than_a_week_from(now, Some(now + one_day)),
            now + one_day
        );
        assert_eq!(
            no_more_than_a_week_from(now, Some(now - one_day)),
            now - one_day
        );
        assert_eq!(
            no_more_than_a_week_from(now, Some(now + 30 * one_day)),
            now + one_day * 7
        );
    }

    /// A fake implementation of DirState that just wants a fixed set
    /// of microdescriptors.  It doesn't care if it gets them: it just
    /// wants to be told that the IDs exist.
    #[derive(Debug, Clone)]
    struct DemoState {
        second_time_around: bool,
        got_items: HashMap<MdDigest, bool>,
    }

    // Constants from Lou Reed
    const H1: MdDigest = *b"satellite's gone up to the skies";
    const H2: MdDigest = *b"things like that drive me out of";
    const H3: MdDigest = *b"my mind i watched it for a littl";
    const H4: MdDigest = *b"while i like to watch things on ";
    const H5: MdDigest = *b"TV Satellite of love Satellite--";

    impl DemoState {
        fn new1() -> Self {
            DemoState {
                second_time_around: false,
                got_items: vec![(H1, false), (H2, false)].into_iter().collect(),
            }
        }
        fn new2() -> Self {
            DemoState {
                second_time_around: true,
                got_items: vec![(H3, false), (H4, false), (H5, false)]
                    .into_iter()
                    .collect(),
            }
        }
        fn n_ready(&self) -> usize {
            self.got_items.values().filter(|x| **x).count()
        }
    }

    impl DirState for DemoState {
        fn describe(&self) -> String {
            format!("{:?}", &self)
        }
        fn bootstrap_status(&self) -> crate::event::DirStatus {
            crate::event::DirStatus::default()
        }
        fn is_ready(&self, ready: Readiness) -> bool {
            match (ready, self.second_time_around) {
                (_, false) => false,
                (Readiness::Complete, true) => self.n_ready() == self.got_items.len(),
                (Readiness::Usable, true) => self.n_ready() >= self.got_items.len() - 1,
            }
        }
        fn can_advance(&self) -> bool {
            if self.second_time_around {
                false
            } else {
                self.n_ready() == self.got_items.len()
            }
        }
        fn missing_docs(&self) -> Vec<DocId> {
            self.got_items
                .iter()
                .filter_map(|(id, have)| {
                    if *have {
                        None
                    } else {
                        Some(DocId::Microdesc(*id))
                    }
                })
                .collect()
        }
        fn add_from_cache(
            &mut self,
            docs: HashMap<DocId, DocumentText>,
            _storage: Option<&Mutex<DynStore>>,
        ) -> Result<bool> {
            let mut changed = false;
            for id in docs.keys() {
                if let DocId::Microdesc(id) = id {
                    if self.got_items.get(id) == Some(&false) {
                        self.got_items.insert(*id, true);
                        changed = true;
                    }
                }
            }
            Ok(changed)
        }
        fn add_from_download(
            &mut self,
            text: &str,
            _request: &ClientRequest,
            _storage: Option<&Mutex<DynStore>>,
        ) -> Result<bool> {
            let mut changed = false;
            for token in text.split_ascii_whitespace() {
                if let Ok(v) = hex::decode(token) {
                    if let Ok(id) = v.try_into() {
                        if self.got_items.get(&id) == Some(&false) {
                            self.got_items.insert(id, true);
                            changed = true;
                        }
                    }
                }
            }
            Ok(changed)
        }
        fn dl_config(&self) -> Result<DownloadSchedule> {
            Ok(DownloadSchedule::default())
        }
        fn advance(self: Box<Self>) -> Result<Box<dyn DirState>> {
            if self.can_advance() {
                Ok(Box::new(Self::new2()))
            } else {
                Ok(self)
            }
        }
        fn reset_time(&self) -> Option<SystemTime> {
            None
        }
        fn reset(self: Box<Self>) -> Result<Box<dyn DirState>> {
            Ok(Box::new(Self::new1()))
        }
    }

    #[test]
    fn all_in_cache() {
        // Let's try bootstrapping when everything is in the cache.
        tor_rtcompat::test_with_one_runtime!(|rt| async {
            let now = rt.wallclock();
            let (_tempdir, mgr) = new_mgr(rt);

            {
                let mut store = mgr.store_if_rw().unwrap().lock().unwrap();
                for h in [H1, H2, H3, H4, H5] {
                    store.store_microdescs(&[("ignore", &h)], now).unwrap();
                }
            }
            let mgr = Arc::new(mgr);

            // Try just a load.
            let state = Box::new(DemoState::new1());
            let result = super::load(Arc::clone(&mgr), state).await.unwrap();
            assert!(result.is_ready(Readiness::Complete));

            // Try a bootstrap that could (but won't!) download.
            let state = Box::new(DemoState::new1());

            let mut on_usable = None;
            let result = super::download(Arc::downgrade(&mgr), state, &mut on_usable)
                .await
                .unwrap();
            assert!(result.0.is_ready(Readiness::Complete));
        });
    }

    #[test]
    fn partly_in_cache() {
        // Let's try bootstrapping with all of phase1 and part of
        // phase 2 in cache.
        tor_rtcompat::test_with_one_runtime!(|rt| async {
            let now = rt.wallclock();
            let (_tempdir, mgr) = new_mgr(rt);

            {
                let mut store = mgr.store_if_rw().unwrap().lock().unwrap();
                for h in [H1, H2, H3] {
                    store.store_microdescs(&[("ignore", &h)], now).unwrap();
                }
            }
            {
                let mut resp = CANNED_RESPONSE.lock().unwrap();
                // H4 and H5.
                *resp = Some(
                    "7768696c652069206c696b6520746f207761746368207468696e6773206f6e20
                     545620536174656c6c697465206f66206c6f766520536174656c6c6974652d2d"
                        .to_owned(),
                );
            }
            let mgr = Arc::new(mgr);
            let mut on_usable = None;

            let state = Box::new(DemoState::new1());
            let result = super::download(Arc::downgrade(&mgr), state, &mut on_usable)
                .await
                .unwrap();
            assert!(result.0.is_ready(Readiness::Complete));
        });
    }
}

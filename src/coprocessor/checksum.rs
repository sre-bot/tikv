// Copyright 2018 TiKV Project Authors. Licensed under Apache-2.0.

use std::vec::IntoIter;

use crc::crc64::{self, Digest, Hasher64};
use kvproto::coprocessor::{KeyRange, Response};
use protobuf::Message;
use tipb::checksum::{ChecksumAlgorithm, ChecksumRequest, ChecksumResponse};

use crate::storage::{Snapshot, SnapshotStore, Statistics};

use crate::coprocessor::dag::Scanner;
use crate::coprocessor::*;

// `ChecksumContext` is used to handle `ChecksumRequest`
pub struct ChecksumContext<S: Snapshot> {
    req: ChecksumRequest,
    store: SnapshotStore<S>,
    ranges: IntoIter<KeyRange>,
    scanner: Option<Scanner<SnapshotStore<S>>>,
    metrics: Statistics,
}

impl<S: Snapshot> ChecksumContext<S> {
    pub fn new(
        req: ChecksumRequest,
        ranges: Vec<KeyRange>,
        snap: S,
        req_ctx: &ReqContext,
    ) -> Result<Self> {
        let store = SnapshotStore::new(
            snap,
            req.get_start_ts(),
            req_ctx.context.get_isolation_level(),
            !req_ctx.context.get_not_fill_cache(),
        );
        Ok(Self {
            req,
            store,
            ranges: ranges.into_iter(),
            scanner: None,
            metrics: Default::default(),
        })
    }

    fn next_row(&mut self) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        loop {
            if let Some(scanner) = self.scanner.as_mut() {
                match scanner.next_row()? {
                    Some(row) => return Ok(Some(row)),
                    None => scanner.collect_statistics_into(&mut self.metrics),
                }
            }

            if let Some(range) = self.ranges.next() {
                self.scanner = match self.scanner.take() {
                    Some(mut scanner) => {
                        box_try!(scanner.reset_range(range, &self.store));
                        Some(scanner)
                    }
                    None => Some(self.new_scanner(range)?),
                };
                continue;
            }

            return Ok(None);
        }
    }

    fn new_scanner(&self, range: KeyRange) -> Result<Scanner<SnapshotStore<S>>> {
        Scanner::new(&self.store, false, false, range).map_err(Error::from)
    }
}

impl<S: Snapshot> RequestHandler for ChecksumContext<S> {
    fn handle_request(&mut self) -> Result<Response> {
        let algorithm = self.req.get_algorithm();
        if algorithm != ChecksumAlgorithm::Crc64_Xor {
            return Err(box_err!("unknown checksum algorithm {:?}", algorithm));
        }

        let mut checksum = 0;
        let mut total_kvs = 0;
        let mut total_bytes = 0;
        while let Some((k, v)) = self.next_row()? {
            checksum = checksum_crc64_xor(checksum, &k, &v);
            total_kvs += 1;
            total_bytes += k.len() + v.len();
        }

        let mut resp = ChecksumResponse::default();
        resp.set_checksum(checksum);
        resp.set_total_kvs(total_kvs);
        resp.set_total_bytes(total_bytes as u64);
        let data = box_try!(resp.write_to_bytes());

        let mut resp = Response::default();
        resp.set_data(data);
        Ok(resp)
    }

    fn collect_scan_statistics(&mut self, dest: &mut Statistics) {
        dest.add(&self.metrics);
        self.metrics = Default::default();
    }
}

fn checksum_crc64_xor(checksum: u64, k: &[u8], v: &[u8]) -> u64 {
    let mut digest = Digest::new(crc64::ECMA);
    digest.write(k);
    digest.write(v);
    checksum ^ digest.sum64()
}

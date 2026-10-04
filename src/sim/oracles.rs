//! Checks of the debugger against the simulation's ground truth.
//!
//! Each oracle states one rule and, when it fails, cites both sides of the
//! disagreement. An oracle is never loosened to make a run pass; one that
//! is wrong is fixed in a commit that explains why.

use std::sync::Arc;

use super::corpus::Run;
use super::kernel::{ExitStatus, Kernel};
use super::loader::Image;
use crate::backend::sim_edge::Truth;

const BREAKPOINT: u8 = 0xcc;

/// Code integrity: every byte of executable memory holds the image's
/// original, or `0xcc` exactly where the controller says it installed a
/// trap over that original.
pub fn code_integrity(kernel: &Kernel, truth: &Truth, image: &Image) -> Result<(), String> {
    let Some(process) = truth.inferior.and_then(|tgid| kernel.processes.get(&tgid)) else {
        return Ok(());
    };
    let code = image.code().collect::<Vec<_>>();
    let original = |address: u64| {
        code.iter().find_map(|(start, page)| {
            let offset = usize::try_from(address.checked_sub(*start)?).ok()?;
            page.get(offset).copied()
        })
    };
    // Every trap the controller installed is in memory, over the byte it
    // remembers.
    for (&address, site) in &truth.sites {
        let Some(byte) = process.space.peek_bytes(address, 1) else {
            continue;
        };
        if site.installed && byte[0] != BREAKPOINT {
            return Err(format!(
                "the controller installed a trap at {address:#x}, but memory holds {:#04x}",
                byte[0]
            ));
        }
        if let Some(was) = original(address)
            && site.original_byte != was
        {
            return Err(format!(
                "the controller remembers {:#04x} under its site at {address:#x}, but the \
                 program has {was:#04x} there",
                site.original_byte
            ));
        }
    }
    // Every other byte of code is the program's own. A page still shared
    // with the image was never written.
    for &(page_address, original_page) in &code {
        if process
            .space
            .page(page_address)
            .is_some_and(|page| Arc::ptr_eq(page, original_page))
        {
            continue;
        }
        let current = process
            .space
            .peek_bytes(page_address, original_page.len())
            .ok_or_else(|| format!("code page {page_address:#x} is no longer mapped"))?;
        if current[..] == original_page[..] {
            continue;
        }
        for (offset, (&now, &was)) in current.iter().zip(original_page.iter()).enumerate() {
            if now == was {
                continue;
            }
            let address = page_address + offset as u64;
            let site = truth.sites.get(&address);
            if now != BREAKPOINT || !site.is_some_and(|site| site.installed) {
                return Err(format!(
                    "code at {address:#x} is {now:#04x}, but the program has {was:#04x} there \
                     and the controller's site is {site:?}"
                ));
            }
        }
    }
    Ok(())
}

/// Breakpoint ownership: every site has an owner, and only the execution
/// in progress owns plan sites. A published stop ended every plan.
pub fn site_ownership(truth: &Truth) -> Result<(), String> {
    for (&address, site) in &truth.sites {
        if site.owners == 0 {
            return Err(format!("site {address:#x} has no owner"));
        }
        for &execution in &site.plans {
            if Some(execution) != truth.active_execution {
                return Err(format!(
                    "execution {execution}'s plan still owns {address:#x}, but the active \
                     execution is {:?}",
                    truth.active_execution
                ));
            }
        }
    }
    if truth.public_stop.is_some() && !truth.plan_sites.is_empty() {
        return Err(format!(
            "stop {:?} is published while plans still record sites: {:?}",
            truth.public_stop, truth.plan_sites
        ));
    }
    Ok(())
}

/// Transparency, while the program runs: what it wrote so far begins what
/// it writes undisturbed.
pub fn output_so_far(kernel: &Kernel, run: &Run) -> Result<(), String> {
    for process in kernel.processes.values() {
        if !run.output.as_bytes().starts_with(&process.output) {
            return Err(format!(
                "the program wrote {:?}, which does not begin {:?}",
                String::from_utf8_lossy(&process.output),
                run.output
            ));
        }
    }
    Ok(())
}

/// Clean exit: when the session is over, no simulated process remains.
pub fn clean_exit(kernel: &Kernel) -> Result<(), String> {
    if let Some(thread) = kernel.threads.values().next() {
        return Err(format!(
            "thread {} of process {} outlived the session, {:?}",
            thread.tid, thread.tgid, thread.state
        ));
    }
    Ok(())
}

/// Transparency, once the program ended: a program that exited by itself
/// did exactly what it does undisturbed, and one killed had written only a
/// beginning of that.
pub fn transparency(kernel: &Kernel, run: &Run) -> Result<(), String> {
    for (tgid, (status, output)) in &kernel.ended {
        if !run.output.as_bytes().starts_with(output) {
            return Err(format!(
                "process {tgid} wrote {:?}, which does not begin {:?}",
                String::from_utf8_lossy(output),
                run.output
            ));
        }
        if let ExitStatus::Code(code) = *status
            && (code & 0xff != run.exit_code || output.as_slice() != run.output.as_bytes())
        {
            return Err(format!(
                "process {tgid} exited {code} after writing {:?}; undisturbed it exits {} \
                 after writing {:?}",
                String::from_utf8_lossy(output),
                run.exit_code,
                run.output
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;
    use crate::backend::sim_edge::Site;
    use crate::sim::corpus::Corpus;

    fn site(original_byte: u8, installed: bool, plans: Vec<u64>) -> Site {
        Site {
            original_byte,
            installed,
            owners: plans.len().max(1),
            plans,
        }
    }

    /// Code integrity accepts a trap the controller installed over the
    /// byte it remembers, and refuses a trap it does not know of and one
    /// over a byte it misremembers.
    #[test]
    fn code_integrity_requires_every_trap_to_be_owned_and_remembered() {
        let corpus = Corpus::load().expect("load the golden corpus");
        let variant = &corpus.programs[0].variants[0];
        let mut kernel = Kernel::new(100);
        let tgid = kernel.spawn(Arc::clone(&variant.image), &variant.path, &[], [0; 16]);
        let entry = variant.image.entry();
        let word = kernel.peek(tgid, entry).expect("read the entry");
        let original = word.to_le_bytes()[0];
        kernel
            .poke(tgid, entry, (word & !0xff) | u64::from(BREAKPOINT))
            .expect("plant a trap");
        let truth = |sites: Vec<(u64, Site)>| Truth {
            sites: sites.into_iter().collect(),
            inferior: Some(tgid),
            ..Truth::default()
        };

        assert_eq!(
            code_integrity(
                &kernel,
                &truth(vec![(entry, site(original, true, vec![]))]),
                &variant.image
            ),
            Ok(())
        );
        let unknown = code_integrity(&kernel, &truth(vec![]), &variant.image);
        assert!(
            unknown.is_err_and(|message| message.contains("program has")),
            "unknown trap"
        );
        let misremembered = code_integrity(
            &kernel,
            &truth(vec![(entry, site(original ^ 1, true, vec![]))]),
            &variant.image,
        );
        assert!(misremembered.is_err_and(|message| message.contains("remembers")));
        let lifted = code_integrity(
            &kernel,
            &truth(vec![(entry, site(original, false, vec![]))]),
            &variant.image,
        );
        assert!(lifted.is_err(), "a lifted site must hold its original byte");
    }

    /// Only the active execution owns plan sites, a published stop ended
    /// every plan, and no site is left without owners.
    #[test]
    fn site_ownership_ends_plans_with_their_execution() {
        let owned = |plans: Vec<u64>| Truth {
            sites: BTreeMap::from([(0x1000, site(0x55, true, plans))]),
            ..Truth::default()
        };
        assert_eq!(
            site_ownership(&Truth {
                active_execution: Some(2),
                ..owned(vec![2])
            }),
            Ok(())
        );
        assert!(
            site_ownership(&Truth {
                active_execution: Some(3),
                ..owned(vec![2])
            })
            .is_err()
        );
        assert!(
            site_ownership(&Truth {
                public_stop: Some(4),
                plan_sites: BTreeMap::from([(2, BTreeSet::from([0x1000]))]),
                ..owned(vec![])
            })
            .is_err()
        );
        let mut orphan = owned(vec![]);
        orphan.sites.get_mut(&0x1000).expect("the site").owners = 0;
        assert!(site_ownership(&orphan).is_err());
    }

    /// A program that exited by itself must have done exactly what it does
    /// undisturbed; a killed one may have done only a beginning of it.
    #[test]
    fn transparency_compares_ends_with_the_manifest() {
        let run = Run {
            arguments: Vec::new(),
            exit_code: 2,
            output: "total 202\n".into(),
        };
        let ended = |status, output: &str| {
            let mut kernel = Kernel::new(100);
            kernel
                .ended
                .insert(1000, (status, output.as_bytes().to_vec()));
            transparency(&kernel, &run)
        };
        assert_eq!(ended(ExitStatus::Code(2), "total 202\n"), Ok(()));
        assert_eq!(ended(ExitStatus::Signal(9, false), "tot"), Ok(()));
        assert!(ended(ExitStatus::Code(3), "total 202\n").is_err());
        assert!(ended(ExitStatus::Code(2), "total 20").is_err());
        assert!(ended(ExitStatus::Signal(9, false), "total 9").is_err());
    }
}

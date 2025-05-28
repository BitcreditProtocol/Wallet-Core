// ----- standard library imports
use std::collections::HashMap;
// ----- extra library imports
// ----- local modules
// ----- end imports

// TODO, optimize by sorting proofs
pub fn select_proofs_for_amount(
    proofs: &[cashu::Proof],
    send_amount: u64,
) -> Option<Vec<cashu::Proof>> {
    let mut dp: HashMap<u64, Vec<cashu::Proof>> = HashMap::new();
    dp.insert(0, Vec::new());

    for proof in proofs {
        let current_dp = dp.clone();
        for (sum, subset) in current_dp.iter() {
            let new_sum = sum + u64::from(proof.amount);
            if new_sum > send_amount {
                continue;
            }
            dp.entry(new_sum).or_insert_with(|| {
                let mut new_subset = subset.clone();
                new_subset.push(proof.clone());
                new_subset
            });
        }
    }

    dp.get(&send_amount).cloned()
}

pub fn generate_blinds(
    keyset_id: cashu::Id,
    amounts: &[cashu::Amount],
) -> Vec<(
    cashu::BlindedMessage,
    cashu::secret::Secret,
    cashu::SecretKey,
)> {
    let mut blinds = Vec::new();
    for amount in amounts {
        let blind = generate_blind(keyset_id, *amount);
        blinds.push(blind);
    }
    blinds
}

pub fn generate_blind(
    kid: cashu::Id,
    amount: cashu::Amount,
) -> (
    cashu::BlindedMessage,
    cashu::secret::Secret,
    cashu::SecretKey,
) {
    let secret = cashu::secret::Secret::new(rand::random::<u64>().to_string());
    let (b_, r) =
        cashu::dhke::blind_message(secret.as_bytes(), None).expect("cdk_dhke::blind_message");
    (cashu::BlindedMessage::new(amount, kid, b_), secret, r)
}

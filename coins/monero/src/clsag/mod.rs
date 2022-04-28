use rand_core::{RngCore, CryptoRng};

use blake2::{Digest, Blake2b512};

use curve25519_dalek::{
  constants::ED25519_BASEPOINT_TABLE,
  scalar::Scalar,
  traits::VartimePrecomputedMultiscalarMul,
  edwards::{EdwardsPoint, VartimeEdwardsPrecomputation}
};

use monero::{
  consensus::Encodable,
  util::ringct::{Key, Clsag}
};

use crate::{
  Commitment,
  transaction::SignableInput,
  c_verify_clsag,
  random_scalar,
  hash_to_scalar,
  hash_to_point
};

#[cfg(feature = "multisig")]
mod multisig;
#[cfg(feature = "multisig")]
pub use multisig::Multisig;

#[allow(non_snake_case)]
pub(crate) fn sign_core(
  rand_source: [u8; 64],
  msg: &[u8; 32],
  input: &SignableInput,
  mask: Scalar,
  A: EdwardsPoint,
  AH: EdwardsPoint
) -> (Clsag, Scalar, Scalar, Scalar, Scalar, EdwardsPoint) {
  let n = input.ring.len();
  let r: usize = input.i.into();

  let C_out;

  let mut P = vec![];
  P.reserve_exact(n);
  let mut C = vec![];
  C.reserve_exact(n);
  let mut C_non_zero = vec![];
  C_non_zero.reserve_exact(n);

  let z;

  let mut next_rand = rand_source;
  next_rand = Blake2b512::digest(&next_rand).as_slice().try_into().unwrap();
  {
    C_out = Commitment::new(mask, input.commitment.amount).calculate();

    for member in &input.ring {
      P.push(member[0]);
      C_non_zero.push(member[1]);
      C.push(C_non_zero[C_non_zero.len() - 1] - C_out);
    }

    z = input.commitment.mask - mask;
  }

  let H = hash_to_point(&P[r]);
  let mut D = H * z;

  // Doesn't use a constant time table as dalek takes longer to generate those then they save
  let images_precomp = VartimeEdwardsPrecomputation::new(&[input.image, D]);
  D = Scalar::from(8 as u8).invert() * D;

  let mut to_hash = vec![];
  to_hash.reserve_exact(((2 * n) + 4) * 32);
  const PREFIX: &str = "CLSAG_";
  const AGG_0: &str =  "CLSAG_agg_0";
  const ROUND: &str =        "round";
  to_hash.extend(AGG_0.bytes());
  to_hash.extend([0; 32 - AGG_0.len()]);

  for i in 0 .. n {
    to_hash.extend(P[i].compress().to_bytes());
  }

  for i in 0 .. n {
    to_hash.extend(C_non_zero[i].compress().to_bytes());
  }

  to_hash.extend(input.image.compress().to_bytes());
  let D_bytes = D.compress().to_bytes();
  to_hash.extend(D_bytes);
  to_hash.extend(C_out.compress().to_bytes());
  let mu_P = hash_to_scalar(&to_hash);
  to_hash[AGG_0.len() - 1] = '1' as u8;
  let mu_C = hash_to_scalar(&to_hash);

  to_hash.truncate(((2 * n) + 1) * 32);
  to_hash.reserve_exact(((2 * n) + 5) * 32);
  for i in 0 .. ROUND.len() {
    to_hash[PREFIX.len() + i] = ROUND.as_bytes()[i] as u8;
  }
  to_hash.extend(C_out.compress().to_bytes());
  to_hash.extend(msg);
  to_hash.extend(A.compress().to_bytes());
  to_hash.extend(AH.compress().to_bytes());
  let mut c = hash_to_scalar(&to_hash);

  let mut c1 = Scalar::zero();
  let mut i = (r + 1) % n;
  if i == 0 {
    c1 = c;
  }

  let mut s = vec![];
  s.resize(n, Scalar::zero());
  while i != r {
    s[i] = Scalar::from_bytes_mod_order_wide(&next_rand);
    next_rand = Blake2b512::digest(&next_rand).as_slice().try_into().unwrap();
    let c_p = mu_P * c;
    let c_c = mu_C * c;

    let L = (&s[i] * &ED25519_BASEPOINT_TABLE) + (c_p * P[i]) + (c_c * C[i]);
    let PH = hash_to_point(&P[i]);
    // Shouldn't be an issue as all of the variables in this vartime statement are public
    let R = (s[i] * PH) + images_precomp.vartime_multiscalar_mul(&[c_p, c_c]);

    to_hash.truncate(((2 * n) + 3) * 32);
    to_hash.extend(L.compress().to_bytes());
    to_hash.extend(R.compress().to_bytes());
    c = hash_to_scalar(&to_hash);

    i = (i + 1) % n;
    if i == 0 {
      c1 = c;
    }
  }

  (
    Clsag {
      s: s.iter().map(|s| Key { key: s.to_bytes() }).collect(),
      c1: Key { key: c1.to_bytes() },
      D: Key { key: D_bytes }
    },
    c, mu_C, z, mu_P,
    C_out
  )
}

#[allow(non_snake_case)]
pub fn sign<R: RngCore + CryptoRng>(
  rng: &mut R,
  msg: [u8; 32],
  inputs: &[(Scalar, SignableInput)],
  sum_outputs: Scalar
) -> Option<Vec<(Clsag, EdwardsPoint)>> {
  if inputs.len() == 0 {
    return None;
  }

  let nonce = random_scalar(rng);
  let mut rand_source = [0; 64];
  rng.fill_bytes(&mut rand_source);

  let mut res = Vec::with_capacity(inputs.len());
  let mut sum_pseudo_outs = Scalar::zero();
  for i in 0 .. inputs.len() {
    let mut mask = random_scalar(rng);
    if i == (inputs.len() - 1) {
      mask = sum_outputs - sum_pseudo_outs;
    } else {
      sum_pseudo_outs += mask;
    }

    let mut rand_source = [0; 64];
    rng.fill_bytes(&mut rand_source);
    let (mut clsag, c, mu_C, z, mu_P, C_out) = sign_core(
      rand_source,
      &msg,
      &inputs[i].1,
      mask,
      &nonce * &ED25519_BASEPOINT_TABLE, nonce * hash_to_point(&inputs[i].1.ring[inputs[i].1.i][0])
    );
    clsag.s[inputs[i].1.i as usize] = Key {
      key: (nonce - (c * ((mu_C * z) + (mu_P * inputs[i].0)))).to_bytes()
    };

    res.push((clsag, C_out));
  }

  Some(res)
}

// Uses Monero's C verification function to ensure compatibility with Monero
pub fn verify(
  clsag: &Clsag,
  image: EdwardsPoint,
  msg: &[u8; 32],
  ring: &[[EdwardsPoint; 2]],
  pseudo_out: EdwardsPoint
) -> bool {
  // Workaround for the fact monero-rs doesn't include the length of clsag.s in clsag encoding
  // despite it being part of clsag encoding. Reason for the patch version pin
  let mut serialized = vec![clsag.s.len() as u8];
  clsag.consensus_encode(&mut serialized).unwrap();

  let image_bytes = image.compress().to_bytes();

  let mut ring_bytes = vec![];
  for member in ring {
    ring_bytes.extend(&member[0].compress().to_bytes());
    ring_bytes.extend(&member[1].compress().to_bytes());
  }

  let pseudo_out_bytes = pseudo_out.compress().to_bytes();

  unsafe {
    c_verify_clsag(
      serialized.len(), serialized.as_ptr(), image_bytes.as_ptr(),
      ring.len() as u8, ring_bytes.as_ptr(), msg.as_ptr(), pseudo_out_bytes.as_ptr()
    )
  }
}

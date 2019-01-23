use ln::channelmanager::{PaymentHash, HTLCSource};
use ln::msgs;
use ln::router::{Route,RouteHop};
use util::{byte_utils, internal_traits};
use util::chacha20::ChaCha20;
use util::errors::{self, APIError};
use util::ser::{Readable, Writeable};
use util::logger::Logger;

use bitcoin_hashes::{Hash, HashEngine};
use bitcoin_hashes::cmp::fixed_time_eq;
use bitcoin_hashes::hmac::{Hmac, HmacEngine};
use bitcoin_hashes::sha256::Hash as Sha256;

use secp256k1::key::{SecretKey,PublicKey};
use secp256k1::Secp256k1;
use secp256k1::ecdh::SharedSecret;
use secp256k1;

use std::ptr;
use std::io::Cursor;
use std::sync::Arc;

pub(super) struct OnionKeys {
	#[cfg(test)]
	pub(super) shared_secret: SharedSecret,
	#[cfg(test)]
	pub(super) blinding_factor: [u8; 32],
	pub(super) ephemeral_pubkey: PublicKey,
	pub(super) rho: [u8; 32],
	pub(super) mu: [u8; 32],
}

#[inline]
pub(super) fn gen_rho_mu_from_shared_secret(shared_secret: &[u8]) -> ([u8; 32], [u8; 32]) {
	assert_eq!(shared_secret.len(), 32);
	({
		let mut hmac = HmacEngine::<Sha256>::new(&[0x72, 0x68, 0x6f]); // rho
		hmac.input(&shared_secret[..]);
		Hmac::from_engine(hmac).into_inner()
	},
	{
		let mut hmac = HmacEngine::<Sha256>::new(&[0x6d, 0x75]); // mu
		hmac.input(&shared_secret[..]);
		Hmac::from_engine(hmac).into_inner()
	})
}

#[inline]
pub(super) fn gen_um_from_shared_secret(shared_secret: &[u8]) -> [u8; 32] {
	assert_eq!(shared_secret.len(), 32);
	let mut hmac = HmacEngine::<Sha256>::new(&[0x75, 0x6d]); // um
	hmac.input(&shared_secret[..]);
	Hmac::from_engine(hmac).into_inner()
}

#[inline]
pub(super) fn gen_ammag_from_shared_secret(shared_secret: &[u8]) -> [u8; 32] {
	assert_eq!(shared_secret.len(), 32);
	let mut hmac = HmacEngine::<Sha256>::new(&[0x61, 0x6d, 0x6d, 0x61, 0x67]); // ammag
	hmac.input(&shared_secret[..]);
	Hmac::from_engine(hmac).into_inner()
}

// can only fail if an intermediary hop has an invalid public key or session_priv is invalid
#[inline]
pub(super) fn construct_onion_keys_callback<T: secp256k1::Signing, FType: FnMut(SharedSecret, [u8; 32], PublicKey, &RouteHop)> (secp_ctx: &Secp256k1<T>, route: &Route, session_priv: &SecretKey, mut callback: FType) -> Result<(), secp256k1::Error> {
	let mut blinded_priv = session_priv.clone();
	let mut blinded_pub = PublicKey::from_secret_key(secp_ctx, &blinded_priv);

	for hop in route.hops.iter() {
		let shared_secret = SharedSecret::new(&hop.pubkey, &blinded_priv);

		let mut sha = Sha256::engine();
		sha.input(&blinded_pub.serialize()[..]);
		sha.input(&shared_secret[..]);
		let blinding_factor = Sha256::from_engine(sha).into_inner();

		let ephemeral_pubkey = blinded_pub;

		blinded_priv.mul_assign(&blinding_factor)?;
		blinded_pub = PublicKey::from_secret_key(secp_ctx, &blinded_priv);

		callback(shared_secret, blinding_factor, ephemeral_pubkey, hop);
	}

	Ok(())
}

// can only fail if an intermediary hop has an invalid public key or session_priv is invalid
pub(super) fn construct_onion_keys<T: secp256k1::Signing>(secp_ctx: &Secp256k1<T>, route: &Route, session_priv: &SecretKey) -> Result<Vec<OnionKeys>, secp256k1::Error> {
	let mut res = Vec::with_capacity(route.hops.len());

	construct_onion_keys_callback(secp_ctx, route, session_priv, |shared_secret, _blinding_factor, ephemeral_pubkey, _| {
		let (rho, mu) = gen_rho_mu_from_shared_secret(&shared_secret[..]);

		res.push(OnionKeys {
			#[cfg(test)]
			shared_secret,
			#[cfg(test)]
			blinding_factor: _blinding_factor,
			ephemeral_pubkey,
			rho,
			mu,
		});
	})?;

	Ok(res)
}

/// returns the hop data, as well as the first-hop value_msat and CLTV value we should send.
pub(super) fn build_onion_payloads(route: &Route, starting_htlc_offset: u32) -> Result<(Vec<msgs::OnionHopData>, u64, u32), APIError> {
	let mut cur_value_msat = 0u64;
	let mut cur_cltv = starting_htlc_offset;
	let mut last_short_channel_id = 0;
	let mut res: Vec<msgs::OnionHopData> = Vec::with_capacity(route.hops.len());
	internal_traits::test_no_dealloc::<msgs::OnionHopData>(None);
	unsafe { res.set_len(route.hops.len()); }

	for (idx, hop) in route.hops.iter().enumerate().rev() {
		// First hop gets special values so that it can check, on receipt, that everything is
		// exactly as it should be (and the next hop isn't trying to probe to find out if we're
		// the intended recipient).
		let value_msat = if cur_value_msat == 0 { hop.fee_msat } else { cur_value_msat };
		let cltv = if cur_cltv == starting_htlc_offset { hop.cltv_expiry_delta + starting_htlc_offset } else { cur_cltv };
		res[idx] = msgs::OnionHopData {
			realm: 0,
			data: msgs::OnionRealm0HopData {
				short_channel_id: last_short_channel_id,
				amt_to_forward: value_msat,
				outgoing_cltv_value: cltv,
			},
			hmac: [0; 32],
		};
		cur_value_msat += hop.fee_msat;
		if cur_value_msat >= 21000000 * 100000000 * 1000 {
			return Err(APIError::RouteError{err: "Channel fees overflowed?!"});
		}
		cur_cltv += hop.cltv_expiry_delta as u32;
		if cur_cltv >= 500000000 {
			return Err(APIError::RouteError{err: "Channel CLTV overflowed?!"});
		}
		last_short_channel_id = hop.short_channel_id;
	}
	Ok((res, cur_value_msat, cur_cltv))
}

#[inline]
fn shift_arr_right(arr: &mut [u8; 20*65]) {
	unsafe {
		ptr::copy(arr[0..].as_ptr(), arr[65..].as_mut_ptr(), 19*65);
	}
	for i in 0..65 {
		arr[i] = 0;
	}
}

#[inline]
fn xor_bufs(dst: &mut[u8], src: &[u8]) {
	assert_eq!(dst.len(), src.len());

	for i in 0..dst.len() {
		dst[i] ^= src[i];
	}
}

const ZERO:[u8; 21*65] = [0; 21*65];
pub(super) fn construct_onion_packet(mut payloads: Vec<msgs::OnionHopData>, onion_keys: Vec<OnionKeys>, associated_data: &PaymentHash) -> msgs::OnionPacket {
	let mut buf = Vec::with_capacity(21*65);
	buf.resize(21*65, 0);

	let filler = {
		let iters = payloads.len() - 1;
		let end_len = iters * 65;
		let mut res = Vec::with_capacity(end_len);
		res.resize(end_len, 0);

		for (i, keys) in onion_keys.iter().enumerate() {
			if i == payloads.len() - 1 { continue; }
			let mut chacha = ChaCha20::new(&keys.rho, &[0u8; 8]);
			chacha.process(&ZERO, &mut buf); // We don't have a seek function :(
			xor_bufs(&mut res[0..(i + 1)*65], &buf[(20 - i)*65..21*65]);
		}
		res
	};

	let mut packet_data = [0; 20*65];
	let mut hmac_res = [0; 32];

	for (i, (payload, keys)) in payloads.iter_mut().zip(onion_keys.iter()).rev().enumerate() {
		shift_arr_right(&mut packet_data);
		payload.hmac = hmac_res;
		packet_data[0..65].copy_from_slice(&payload.encode()[..]);

		let mut chacha = ChaCha20::new(&keys.rho, &[0u8; 8]);
		chacha.process(&packet_data, &mut buf[0..20*65]);
		packet_data[..].copy_from_slice(&buf[0..20*65]);

		if i == 0 {
			packet_data[20*65 - filler.len()..20*65].copy_from_slice(&filler[..]);
		}

		let mut hmac = HmacEngine::<Sha256>::new(&keys.mu);
		hmac.input(&packet_data);
		hmac.input(&associated_data.0[..]);
		hmac_res = Hmac::from_engine(hmac).into_inner();
	}

	msgs::OnionPacket{
		version: 0,
		public_key: Ok(onion_keys.first().unwrap().ephemeral_pubkey),
		hop_data: packet_data,
		hmac: hmac_res,
	}
}

/// Encrypts a failure packet. raw_packet can either be a
/// msgs::DecodedOnionErrorPacket.encode() result or a msgs::OnionErrorPacket.data element.
pub(super) fn encrypt_failure_packet(shared_secret: &[u8], raw_packet: &[u8]) -> msgs::OnionErrorPacket {
	let ammag = gen_ammag_from_shared_secret(&shared_secret);

	let mut packet_crypted = Vec::with_capacity(raw_packet.len());
	packet_crypted.resize(raw_packet.len(), 0);
	let mut chacha = ChaCha20::new(&ammag, &[0u8; 8]);
	chacha.process(&raw_packet, &mut packet_crypted[..]);
	msgs::OnionErrorPacket {
		data: packet_crypted,
	}
}

pub(super) fn build_failure_packet(shared_secret: &[u8], failure_type: u16, failure_data: &[u8]) -> msgs::DecodedOnionErrorPacket {
	assert_eq!(shared_secret.len(), 32);
	assert!(failure_data.len() <= 256 - 2);

	let um = gen_um_from_shared_secret(&shared_secret);

	let failuremsg = {
		let mut res = Vec::with_capacity(2 + failure_data.len());
		res.push(((failure_type >> 8) & 0xff) as u8);
		res.push(((failure_type >> 0) & 0xff) as u8);
		res.extend_from_slice(&failure_data[..]);
		res
	};
	let pad = {
		let mut res = Vec::with_capacity(256 - 2 - failure_data.len());
		res.resize(256 - 2 - failure_data.len(), 0);
		res
	};
	let mut packet = msgs::DecodedOnionErrorPacket {
		hmac: [0; 32],
		failuremsg: failuremsg,
		pad: pad,
	};

	let mut hmac = HmacEngine::<Sha256>::new(&um);
	hmac.input(&packet.encode()[32..]);
	packet.hmac = Hmac::from_engine(hmac).into_inner();

	packet
}

#[inline]
pub(super) fn build_first_hop_failure_packet(shared_secret: &[u8], failure_type: u16, failure_data: &[u8]) -> msgs::OnionErrorPacket {
	let failure_packet = build_failure_packet(shared_secret, failure_type, failure_data);
	encrypt_failure_packet(shared_secret, &failure_packet.encode()[..])
}

struct LogHolder<'a> { logger: &'a Arc<Logger> }
/// Process failure we got back from upstream on a payment we sent (implying htlc_source is an
/// OutboundRoute).
/// Returns update, a boolean indicating that the payment itself failed, and the error code.
pub(super) fn process_onion_failure<T: secp256k1::Signing>(secp_ctx: &Secp256k1<T>, logger: &Arc<Logger>, htlc_source: &HTLCSource, mut packet_decrypted: Vec<u8>) -> (Option<msgs::HTLCFailChannelUpdate>, bool, Option<u16>) {
	if let &HTLCSource::OutboundRoute { ref route, ref session_priv, ref first_hop_htlc_msat } = htlc_source {
		let mut res = None;
		let mut htlc_msat = *first_hop_htlc_msat;
		let mut error_code_ret = None;
		let mut next_route_hop_ix = 0;
		let mut is_from_final_node = false;

		// Handle packed channel/node updates for passing back for the route handler
		construct_onion_keys_callback(secp_ctx, route, session_priv, |shared_secret, _, _, route_hop| {
			next_route_hop_ix += 1;
			if res.is_some() { return; }

			let amt_to_forward = htlc_msat - route_hop.fee_msat;
			htlc_msat = amt_to_forward;

			let ammag = gen_ammag_from_shared_secret(&shared_secret[..]);

			let mut decryption_tmp = Vec::with_capacity(packet_decrypted.len());
			decryption_tmp.resize(packet_decrypted.len(), 0);
			let mut chacha = ChaCha20::new(&ammag, &[0u8; 8]);
			chacha.process(&packet_decrypted, &mut decryption_tmp[..]);
			packet_decrypted = decryption_tmp;

			is_from_final_node = route.hops.last().unwrap().pubkey == route_hop.pubkey;

			if let Ok(err_packet) = msgs::DecodedOnionErrorPacket::read(&mut Cursor::new(&packet_decrypted)) {
				let um = gen_um_from_shared_secret(&shared_secret[..]);
				let mut hmac = HmacEngine::<Sha256>::new(&um);
				hmac.input(&err_packet.encode()[32..]);

				if fixed_time_eq(&Hmac::from_engine(hmac).into_inner(), &err_packet.hmac) {
					if let Some(error_code_slice) = err_packet.failuremsg.get(0..2) {
						const PERM: u16 = 0x4000;
						const NODE: u16 = 0x2000;
						const UPDATE: u16 = 0x1000;

						let error_code = byte_utils::slice_to_be16(&error_code_slice);
						error_code_ret = Some(error_code);

						let (debug_field, debug_field_size) = errors::get_onion_debug_field(error_code);

						// indicate that payment parameter has failed and no need to
						// update Route object
						let payment_failed = (match error_code & 0xff {
							15|16|17|18|19 => true,
							_ => false,
						} && is_from_final_node) // PERM bit observed below even this error is from the intermediate nodes
						|| error_code == 21; // Special case error 21 as the Route object is bogus, TODO: Maybe fail the node if the CLTV was reasonable?

						let mut fail_channel_update = None;

						if error_code & NODE == NODE {
							fail_channel_update = Some(msgs::HTLCFailChannelUpdate::NodeFailure { node_id: route_hop.pubkey, is_permanent: error_code & PERM == PERM });
						}
						else if error_code & PERM == PERM {
							fail_channel_update = if payment_failed {None} else {Some(msgs::HTLCFailChannelUpdate::ChannelClosed {
								short_channel_id: route.hops[next_route_hop_ix - if next_route_hop_ix == route.hops.len() { 1 } else { 0 }].short_channel_id,
								is_permanent: true,
							})};
						}
						else if error_code & UPDATE == UPDATE {
							if let Some(update_len_slice) = err_packet.failuremsg.get(debug_field_size+2..debug_field_size+4) {
								let update_len = byte_utils::slice_to_be16(&update_len_slice) as usize;
								if let Some(update_slice) = err_packet.failuremsg.get(debug_field_size + 4..debug_field_size + 4 + update_len) {
									if let Ok(chan_update) = msgs::ChannelUpdate::read(&mut Cursor::new(&update_slice)) {
										// if channel_update should NOT have caused the failure:
										// MAY treat the channel_update as invalid.
										let is_chan_update_invalid = match error_code & 0xff {
											7 => false,
											11 => amt_to_forward > chan_update.contents.htlc_minimum_msat,
											12 => {
												let new_fee = amt_to_forward.checked_mul(chan_update.contents.fee_proportional_millionths as u64).and_then(|prop_fee| { (prop_fee / 1000000).checked_add(chan_update.contents.fee_base_msat as u64) });
												new_fee.is_some() && route_hop.fee_msat >= new_fee.unwrap()
											}
											13 => route_hop.cltv_expiry_delta as u16 >= chan_update.contents.cltv_expiry_delta,
											14 => false, // expiry_too_soon; always valid?
											20 => chan_update.contents.flags & 2 == 0,
											_ => false, // unknown error code; take channel_update as valid
										};
										fail_channel_update = if is_chan_update_invalid {
											// This probably indicates the node which forwarded
											// to the node in question corrupted something.
											Some(msgs::HTLCFailChannelUpdate::ChannelClosed {
												short_channel_id: route_hop.short_channel_id,
												is_permanent: true,
											})
										} else {
											Some(msgs::HTLCFailChannelUpdate::ChannelUpdateMessage {
												msg: chan_update,
											})
										};
									}
								}
							}
							if fail_channel_update.is_none() {
								// They provided an UPDATE which was obviously bogus, not worth
								// trying to relay through them anymore.
								fail_channel_update = Some(msgs::HTLCFailChannelUpdate::NodeFailure {
									node_id: route_hop.pubkey,
									is_permanent: true,
								});
							}
						} else if !payment_failed {
							// We can't understand their error messages and they failed to
							// forward...they probably can't understand our forwards so its
							// really not worth trying any further.
							fail_channel_update = Some(msgs::HTLCFailChannelUpdate::NodeFailure {
								node_id: route_hop.pubkey,
								is_permanent: true,
							});
						}

						// TODO: Here (and a few other places) we assume that BADONION errors
						// are always "sourced" from the node previous to the one which failed
						// to decode the onion.
						res = Some((fail_channel_update, !(error_code & PERM == PERM && is_from_final_node)));

						let (description, title) = errors::get_onion_error_description(error_code);
						if debug_field_size > 0 && err_packet.failuremsg.len() >= 4 + debug_field_size {
							let log_holder = LogHolder { logger };
							log_warn!(log_holder, "Onion Error[{}({:#x}) {}({})] {}", title, error_code, debug_field, log_bytes!(&err_packet.failuremsg[4..4+debug_field_size]), description);
						}
						else {
							let log_holder = LogHolder { logger };
							log_warn!(log_holder, "Onion Error[{}({:#x})] {}", title, error_code, description);
						}
					} else {
						// Useless packet that we can't use but it passed HMAC, so it
						// definitely came from the peer in question
						res = Some((Some(msgs::HTLCFailChannelUpdate::NodeFailure {
							node_id: route_hop.pubkey,
							is_permanent: true,
						}), !is_from_final_node));
					}
				}
			}
		}).expect("Route that we sent via spontaneously grew invalid keys in the middle of it?");
		if let Some((channel_update, payment_retryable)) = res {
			(channel_update, payment_retryable, error_code_ret)
		} else {
			// only not set either packet unparseable or hmac does not match with any
			// payment not retryable only when garbage is from the final node
			(None, !is_from_final_node, None)
		}
	} else { unreachable!(); }
}

#[cfg(test)]
mod tests {
	use ln::channelmanager::PaymentHash;
	use ln::router::{Route, RouteHop};
	use ln::msgs;
	use util::ser::Writeable;

	use hex;

	use secp256k1::Secp256k1;
	use secp256k1::key::{PublicKey,SecretKey};

	use super::OnionKeys;

	fn build_test_onion_keys() -> Vec<OnionKeys> {
		// Keys from BOLT 4, used in both test vector tests
		let secp_ctx = Secp256k1::new();

		let route = Route {
			hops: vec!(
					RouteHop {
						pubkey: PublicKey::from_slice(&hex::decode("02eec7245d6b7d2ccb30380bfbe2a3648cd7a942653f5aa340edcea1f283686619").unwrap()[..]).unwrap(),
						short_channel_id: 0, fee_msat: 0, cltv_expiry_delta: 0 // Test vectors are garbage and not generateble from a RouteHop, we fill in payloads manually
					},
					RouteHop {
						pubkey: PublicKey::from_slice(&hex::decode("0324653eac434488002cc06bbfb7f10fe18991e35f9fe4302dbea6d2353dc0ab1c").unwrap()[..]).unwrap(),
						short_channel_id: 0, fee_msat: 0, cltv_expiry_delta: 0 // Test vectors are garbage and not generateble from a RouteHop, we fill in payloads manually
					},
					RouteHop {
						pubkey: PublicKey::from_slice(&hex::decode("027f31ebc5462c1fdce1b737ecff52d37d75dea43ce11c74d25aa297165faa2007").unwrap()[..]).unwrap(),
						short_channel_id: 0, fee_msat: 0, cltv_expiry_delta: 0 // Test vectors are garbage and not generateble from a RouteHop, we fill in payloads manually
					},
					RouteHop {
						pubkey: PublicKey::from_slice(&hex::decode("032c0b7cf95324a07d05398b240174dc0c2be444d96b159aa6c7f7b1e668680991").unwrap()[..]).unwrap(),
						short_channel_id: 0, fee_msat: 0, cltv_expiry_delta: 0 // Test vectors are garbage and not generateble from a RouteHop, we fill in payloads manually
					},
					RouteHop {
						pubkey: PublicKey::from_slice(&hex::decode("02edabbd16b41c8371b92ef2f04c1185b4f03b6dcd52ba9b78d9d7c89c8f221145").unwrap()[..]).unwrap(),
						short_channel_id: 0, fee_msat: 0, cltv_expiry_delta: 0 // Test vectors are garbage and not generateble from a RouteHop, we fill in payloads manually
					},
			),
		};

		let session_priv = SecretKey::from_slice(&hex::decode("4141414141414141414141414141414141414141414141414141414141414141").unwrap()[..]).unwrap();

		let onion_keys = super::construct_onion_keys(&secp_ctx, &route, &session_priv).unwrap();
		assert_eq!(onion_keys.len(), route.hops.len());
		onion_keys
	}

	#[test]
	fn onion_vectors() {
		// Packet creation test vectors from BOLT 4
		let onion_keys = build_test_onion_keys();

		assert_eq!(onion_keys[0].shared_secret[..], hex::decode("53eb63ea8a3fec3b3cd433b85cd62a4b145e1dda09391b348c4e1cd36a03ea66").unwrap()[..]);
		assert_eq!(onion_keys[0].blinding_factor[..], hex::decode("2ec2e5da605776054187180343287683aa6a51b4b1c04d6dd49c45d8cffb3c36").unwrap()[..]);
		assert_eq!(onion_keys[0].ephemeral_pubkey.serialize()[..], hex::decode("02eec7245d6b7d2ccb30380bfbe2a3648cd7a942653f5aa340edcea1f283686619").unwrap()[..]);
		assert_eq!(onion_keys[0].rho, hex::decode("ce496ec94def95aadd4bec15cdb41a740c9f2b62347c4917325fcc6fb0453986").unwrap()[..]);
		assert_eq!(onion_keys[0].mu, hex::decode("b57061dc6d0a2b9f261ac410c8b26d64ac5506cbba30267a649c28c179400eba").unwrap()[..]);

		assert_eq!(onion_keys[1].shared_secret[..], hex::decode("a6519e98832a0b179f62123b3567c106db99ee37bef036e783263602f3488fae").unwrap()[..]);
		assert_eq!(onion_keys[1].blinding_factor[..], hex::decode("bf66c28bc22e598cfd574a1931a2bafbca09163df2261e6d0056b2610dab938f").unwrap()[..]);
		assert_eq!(onion_keys[1].ephemeral_pubkey.serialize()[..], hex::decode("028f9438bfbf7feac2e108d677e3a82da596be706cc1cf342b75c7b7e22bf4e6e2").unwrap()[..]);
		assert_eq!(onion_keys[1].rho, hex::decode("450ffcabc6449094918ebe13d4f03e433d20a3d28a768203337bc40b6e4b2c59").unwrap()[..]);
		assert_eq!(onion_keys[1].mu, hex::decode("05ed2b4a3fb023c2ff5dd6ed4b9b6ea7383f5cfe9d59c11d121ec2c81ca2eea9").unwrap()[..]);

		assert_eq!(onion_keys[2].shared_secret[..], hex::decode("3a6b412548762f0dbccce5c7ae7bb8147d1caf9b5471c34120b30bc9c04891cc").unwrap()[..]);
		assert_eq!(onion_keys[2].blinding_factor[..], hex::decode("a1f2dadd184eb1627049673f18c6325814384facdee5bfd935d9cb031a1698a5").unwrap()[..]);
		assert_eq!(onion_keys[2].ephemeral_pubkey.serialize()[..], hex::decode("03bfd8225241ea71cd0843db7709f4c222f62ff2d4516fd38b39914ab6b83e0da0").unwrap()[..]);
		assert_eq!(onion_keys[2].rho, hex::decode("11bf5c4f960239cb37833936aa3d02cea82c0f39fd35f566109c41f9eac8deea").unwrap()[..]);
		assert_eq!(onion_keys[2].mu, hex::decode("caafe2820fa00eb2eeb78695ae452eba38f5a53ed6d53518c5c6edf76f3f5b78").unwrap()[..]);

		assert_eq!(onion_keys[3].shared_secret[..], hex::decode("21e13c2d7cfe7e18836df50872466117a295783ab8aab0e7ecc8c725503ad02d").unwrap()[..]);
		assert_eq!(onion_keys[3].blinding_factor[..], hex::decode("7cfe0b699f35525029ae0fa437c69d0f20f7ed4e3916133f9cacbb13c82ff262").unwrap()[..]);
		assert_eq!(onion_keys[3].ephemeral_pubkey.serialize()[..], hex::decode("031dde6926381289671300239ea8e57ffaf9bebd05b9a5b95beaf07af05cd43595").unwrap()[..]);
		assert_eq!(onion_keys[3].rho, hex::decode("cbe784ab745c13ff5cffc2fbe3e84424aa0fd669b8ead4ee562901a4a4e89e9e").unwrap()[..]);
		assert_eq!(onion_keys[3].mu, hex::decode("5052aa1b3d9f0655a0932e50d42f0c9ba0705142c25d225515c45f47c0036ee9").unwrap()[..]);

		assert_eq!(onion_keys[4].shared_secret[..], hex::decode("b5756b9b542727dbafc6765a49488b023a725d631af688fc031217e90770c328").unwrap()[..]);
		assert_eq!(onion_keys[4].blinding_factor[..], hex::decode("c96e00dddaf57e7edcd4fb5954be5b65b09f17cb6d20651b4e90315be5779205").unwrap()[..]);
		assert_eq!(onion_keys[4].ephemeral_pubkey.serialize()[..], hex::decode("03a214ebd875aab6ddfd77f22c5e7311d7f77f17a169e599f157bbcdae8bf071f4").unwrap()[..]);
		assert_eq!(onion_keys[4].rho, hex::decode("034e18b8cc718e8af6339106e706c52d8df89e2b1f7e9142d996acf88df8799b").unwrap()[..]);
		assert_eq!(onion_keys[4].mu, hex::decode("8e45e5c61c2b24cb6382444db6698727afb063adecd72aada233d4bf273d975a").unwrap()[..]);

		// Test vectors below are flat-out wrong: they claim to set outgoing_cltv_value to non-0 :/
		let payloads = vec!(
			msgs::OnionHopData {
				realm: 0,
				data: msgs::OnionRealm0HopData {
					short_channel_id: 0,
					amt_to_forward: 0,
					outgoing_cltv_value: 0,
				},
				hmac: [0; 32],
			},
			msgs::OnionHopData {
				realm: 0,
				data: msgs::OnionRealm0HopData {
					short_channel_id: 0x0101010101010101,
					amt_to_forward: 0x0100000001,
					outgoing_cltv_value: 0,
				},
				hmac: [0; 32],
			},
			msgs::OnionHopData {
				realm: 0,
				data: msgs::OnionRealm0HopData {
					short_channel_id: 0x0202020202020202,
					amt_to_forward: 0x0200000002,
					outgoing_cltv_value: 0,
				},
				hmac: [0; 32],
			},
			msgs::OnionHopData {
				realm: 0,
				data: msgs::OnionRealm0HopData {
					short_channel_id: 0x0303030303030303,
					amt_to_forward: 0x0300000003,
					outgoing_cltv_value: 0,
				},
				hmac: [0; 32],
			},
			msgs::OnionHopData {
				realm: 0,
				data: msgs::OnionRealm0HopData {
					short_channel_id: 0x0404040404040404,
					amt_to_forward: 0x0400000004,
					outgoing_cltv_value: 0,
				},
				hmac: [0; 32],
			},
		);

		let packet = super::construct_onion_packet(payloads, onion_keys, &PaymentHash([0x42; 32]));
		// Just check the final packet encoding, as it includes all the per-hop vectors in it
		// anyway...
		assert_eq!(packet.encode(), hex::decode("0002eec7245d6b7d2ccb30380bfbe2a3648cd7a942653f5aa340edcea1f283686619e5f14350c2a76fc232b5e46d421e9615471ab9e0bc887beff8c95fdb878f7b3a716a996c7845c93d90e4ecbb9bde4ece2f69425c99e4bc820e44485455f135edc0d10f7d61ab590531cf08000179a333a347f8b4072f216400406bdf3bf038659793d4a1fd7b246979e3150a0a4cb052c9ec69acf0f48c3d39cd55675fe717cb7d80ce721caad69320c3a469a202f1e468c67eaf7a7cd8226d0fd32f7b48084dca885d56047694762b67021713ca673929c163ec36e04e40ca8e1c6d17569419d3039d9a1ec866abe044a9ad635778b961fc0776dc832b3a451bd5d35072d2269cf9b040f6b7a7dad84fb114ed413b1426cb96ceaf83825665ed5a1d002c1687f92465b49ed4c7f0218ff8c6c7dd7221d589c65b3b9aaa71a41484b122846c7c7b57e02e679ea8469b70e14fe4f70fee4d87b910cf144be6fe48eef24da475c0b0bcc6565ae82cd3f4e3b24c76eaa5616c6111343306ab35c1fe5ca4a77c0e314ed7dba39d6f1e0de791719c241a939cc493bea2bae1c1e932679ea94d29084278513c77b899cc98059d06a27d171b0dbdf6bee13ddc4fc17a0c4d2827d488436b57baa167544138ca2e64a11b43ac8a06cd0c2fba2d4d900ed2d9205305e2d7383cc98dacb078133de5f6fb6bed2ef26ba92cea28aafc3b9948dd9ae5559e8bd6920b8cea462aa445ca6a95e0e7ba52961b181c79e73bd581821df2b10173727a810c92b83b5ba4a0403eb710d2ca10689a35bec6c3a708e9e92f7d78ff3c5d9989574b00c6736f84c199256e76e19e78f0c98a9d580b4a658c84fc8f2096c2fbea8f5f8c59d0fdacb3be2802ef802abbecb3aba4acaac69a0e965abd8981e9896b1f6ef9d60f7a164b371af869fd0e48073742825e9434fc54da837e120266d53302954843538ea7c6c3dbfb4ff3b2fdbe244437f2a153ccf7bdb4c92aa08102d4f3cff2ae5ef86fab4653595e6a5837fa2f3e29f27a9cde5966843fb847a4a61f1e76c281fe8bb2b0a181d096100db5a1a5ce7a910238251a43ca556712eaadea167fb4d7d75825e440f3ecd782036d7574df8bceacb397abefc5f5254d2722215c53ff54af8299aaaad642c6d72a14d27882d9bbd539e1cc7a527526ba89b8c037ad09120e98ab042d3e8652b31ae0e478516bfaf88efca9f3676ffe99d2819dcaeb7610a626695f53117665d267d3f7abebd6bbd6733f645c72c389f03855bdf1e4b8075b516569b118233a0f0971d24b83113c0b096f5216a207ca99a7cddc81c130923fe3d91e7508c9ac5f2e914ff5dccab9e558566fa14efb34ac98d878580814b94b73acbfde9072f30b881f7f0fff42d4045d1ace6322d86a97d164aa84d93a60498065cc7c20e636f5862dc81531a88c60305a2e59a985be327a6902e4bed986dbf4a0b50c217af0ea7fdf9ab37f9ea1a1aaa72f54cf40154ea9b269f1a7c09f9f43245109431a175d50e2db0132337baa0ef97eed0fcf20489da36b79a1172faccc2f7ded7c60e00694282d93359c4682135642bc81f433574aa8ef0c97b4ade7ca372c5ffc23c7eddd839bab4e0f14d6df15c9dbeab176bec8b5701cf054eb3072f6dadc98f88819042bf10c407516ee58bce33fbe3b3d86a54255e577db4598e30a135361528c101683a5fcde7e8ba53f3456254be8f45fe3a56120ae96ea3773631fcb3873aa3abd91bcff00bd38bd43697a2e789e00da6077482e7b1b1a677b5afae4c54e6cbdf7377b694eb7d7a5b913476a5be923322d3de06060fd5e819635232a2cf4f0731da13b8546d1d6d4f8d75b9fce6c2341a71b0ea6f780df54bfdb0dd5cd9855179f602f9172307c7268724c3618e6817abd793adc214a0dc0bc616816632f27ea336fb56dfd").unwrap());
	}

	#[test]
	fn test_failure_packet_onion() {
		// Returning Errors test vectors from BOLT 4

		let onion_keys = build_test_onion_keys();
		let onion_error = super::build_failure_packet(&onion_keys[4].shared_secret[..], 0x2002, &[0; 0]);
		assert_eq!(onion_error.encode(), hex::decode("4c2fc8bc08510334b6833ad9c3e79cd1b52ae59dfe5c2a4b23ead50f09f7ee0b0002200200fe0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000").unwrap());

		let onion_packet_1 = super::encrypt_failure_packet(&onion_keys[4].shared_secret[..], &onion_error.encode()[..]);
		assert_eq!(onion_packet_1.data, hex::decode("a5e6bd0c74cb347f10cce367f949098f2457d14c046fd8a22cb96efb30b0fdcda8cb9168b50f2fd45edd73c1b0c8b33002df376801ff58aaa94000bf8a86f92620f343baef38a580102395ae3abf9128d1047a0736ff9b83d456740ebbb4aeb3aa9737f18fb4afb4aa074fb26c4d702f42968888550a3bded8c05247e045b866baef0499f079fdaeef6538f31d44deafffdfd3afa2fb4ca9082b8f1c465371a9894dd8c243fb4847e004f5256b3e90e2edde4c9fb3082ddfe4d1e734cacd96ef0706bf63c9984e22dc98851bcccd1c3494351feb458c9c6af41c0044bea3c47552b1d992ae542b17a2d0bba1a096c78d169034ecb55b6e3a7263c26017f033031228833c1daefc0dedb8cf7c3e37c9c37ebfe42f3225c326e8bcfd338804c145b16e34e4").unwrap());

		let onion_packet_2 = super::encrypt_failure_packet(&onion_keys[3].shared_secret[..], &onion_packet_1.data[..]);
		assert_eq!(onion_packet_2.data, hex::decode("c49a1ce81680f78f5f2000cda36268de34a3f0a0662f55b4e837c83a8773c22aa081bab1616a0011585323930fa5b9fae0c85770a2279ff59ec427ad1bbff9001c0cd1497004bd2a0f68b50704cf6d6a4bf3c8b6a0833399a24b3456961ba00736785112594f65b6b2d44d9f5ea4e49b5e1ec2af978cbe31c67114440ac51a62081df0ed46d4a3df295da0b0fe25c0115019f03f15ec86fabb4c852f83449e812f141a9395b3f70b766ebbd4ec2fae2b6955bd8f32684c15abfe8fd3a6261e52650e8807a92158d9f1463261a925e4bfba44bd20b166d532f0017185c3a6ac7957adefe45559e3072c8dc35abeba835a8cb01a71a15c736911126f27d46a36168ca5ef7dccd4e2886212602b181463e0dd30185c96348f9743a02aca8ec27c0b90dca270").unwrap());

		let onion_packet_3 = super::encrypt_failure_packet(&onion_keys[2].shared_secret[..], &onion_packet_2.data[..]);
		assert_eq!(onion_packet_3.data, hex::decode("a5d3e8634cfe78b2307d87c6d90be6fe7855b4f2cc9b1dfb19e92e4b79103f61ff9ac25f412ddfb7466e74f81b3e545563cdd8f5524dae873de61d7bdfccd496af2584930d2b566b4f8d3881f8c043df92224f38cf094cfc09d92655989531524593ec6d6caec1863bdfaa79229b5020acc034cd6deeea1021c50586947b9b8e6faa83b81fbfa6133c0af5d6b07c017f7158fa94f0d206baf12dda6b68f785b773b360fd0497e16cc402d779c8d48d0fa6315536ef0660f3f4e1865f5b38ea49c7da4fd959de4e83ff3ab686f059a45c65ba2af4a6a79166aa0f496bf04d06987b6d2ea205bdb0d347718b9aeff5b61dfff344993a275b79717cd815b6ad4c0beb568c4ac9c36ff1c315ec1119a1993c4b61e6eaa0375e0aaf738ac691abd3263bf937e3").unwrap());

		let onion_packet_4 = super::encrypt_failure_packet(&onion_keys[1].shared_secret[..], &onion_packet_3.data[..]);
		assert_eq!(onion_packet_4.data, hex::decode("aac3200c4968f56b21f53e5e374e3a2383ad2b1b6501bbcc45abc31e59b26881b7dfadbb56ec8dae8857add94e6702fb4c3a4de22e2e669e1ed926b04447fc73034bb730f4932acd62727b75348a648a1128744657ca6a4e713b9b646c3ca66cac02cdab44dd3439890ef3aaf61708714f7375349b8da541b2548d452d84de7084bb95b3ac2345201d624d31f4d52078aa0fa05a88b4e20202bd2b86ac5b52919ea305a8949de95e935eed0319cf3cf19ebea61d76ba92532497fcdc9411d06bcd4275094d0a4a3c5d3a945e43305a5a9256e333e1f64dbca5fcd4e03a39b9012d197506e06f29339dfee3331995b21615337ae060233d39befea925cc262873e0530408e6990f1cbd233a150ef7b004ff6166c70c68d9f8c853c1abca640b8660db2921").unwrap());

		let onion_packet_5 = super::encrypt_failure_packet(&onion_keys[0].shared_secret[..], &onion_packet_4.data[..]);
		assert_eq!(onion_packet_5.data, hex::decode("9c5add3963fc7f6ed7f148623c84134b5647e1306419dbe2174e523fa9e2fbed3a06a19f899145610741c83ad40b7712aefaddec8c6baf7325d92ea4ca4d1df8bce517f7e54554608bf2bd8071a4f52a7a2f7ffbb1413edad81eeea5785aa9d990f2865dc23b4bc3c301a94eec4eabebca66be5cf638f693ec256aec514620cc28ee4a94bd9565bc4d4962b9d3641d4278fb319ed2b84de5b665f307a2db0f7fbb757366067d88c50f7e829138fde4f78d39b5b5802f1b92a8a820865af5cc79f9f30bc3f461c66af95d13e5e1f0381c184572a91dee1c849048a647a1158cf884064deddbf1b0b88dfe2f791428d0ba0f6fb2f04e14081f69165ae66d9297c118f0907705c9c4954a199bae0bb96fad763d690e7daa6cfda59ba7f2c8d11448b604d12d").unwrap());
	}
}
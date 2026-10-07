//! `crypto.subtle`: known answers and round trips.

use std::rc::Rc;

use catpaw_bindings_boa::BoaPage;
use catpaw_web::event_loop::{self, LoopLimits, StopReason};
use catpaw_web::{PageConfig, PageState, scripting};
use url::Url;

fn run(script: &str) -> BoaPage {
    let html = format!(
        "<script>window.out = {{}}; window.failure = null; (async () => {{ try {{ {script} }} catch (e) {{ failure = e.name + ': ' + e.message; }} }})();</script>"
    );
    let state = Rc::new(PageState::new(
        Url::parse("https://example.test/").unwrap(),
        PageConfig::default(),
    ));
    let mut page = BoaPage::new(state).expect("page setup");
    let report = page.with_cx(|cx| {
        scripting::load_document(cx, &html);
        event_loop::run(cx, &LoopLimits::default())
    });
    assert_eq!(report.stop, StopReason::Idle);
    let failure = page.eval_to_string("String(failure)").unwrap();
    assert_eq!(failure, "null", "the script failed: {failure}");
    page
}

fn get(page: &mut BoaPage, key: &str) -> String {
    page.eval_to_string(&format!("String(out.{key})")).unwrap()
}

const HELPERS: &str = r#"
  const enc = s => new TextEncoder().encode(s);
  const hex = b => Array.from(new Uint8Array(b)).map(x => x.toString(16).padStart(2, '0')).join('');
  const unhex = h => new Uint8Array(h.match(/../g).map(x => parseInt(x, 16)));
"#;

#[test]
fn digests_hmac_and_kdfs_match_known_answers() {
    let mut page = run(&format!(
        r#"{HELPERS}
      out.sha256 = hex(await crypto.subtle.digest('SHA-256', enc('abc')));
      const hk = await crypto.subtle.importKey('raw', enc('key'), {{ name: 'HMAC', hash: 'SHA-256' }}, true, ['sign', 'verify']);
      const mac = await crypto.subtle.sign('HMAC', hk, enc('The quick brown fox jumps over the lazy dog'));
      out.hmac = hex(mac);
      out.hmacOk = await crypto.subtle.verify('HMAC', hk, mac, enc('The quick brown fox jumps over the lazy dog'));
      out.hmacBad = await crypto.subtle.verify('HMAC', hk, mac, enc('tampered'));
      const jwk = await crypto.subtle.exportKey('jwk', hk);
      out.jwk = jwk.kty + ' ' + jwk.alg + ' ' + jwk.k + ' ' + jwk.ext + ' ' + jwk.key_ops.join(',');
      out.alg = JSON.stringify(hk.algorithm) + ' ' + hk.type + ' ' + hk.usages.join(',');
      const pw = await crypto.subtle.importKey('raw', enc('password'), 'PBKDF2', false, ['deriveBits', 'deriveKey']);
      out.pbkdf2 = hex(await crypto.subtle.deriveBits({{ name: 'PBKDF2', hash: 'SHA-256', salt: enc('salt'), iterations: 1 }}, pw, 256));
      const ikm = await crypto.subtle.importKey('raw', unhex('0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b'), 'HKDF', false, ['deriveBits']);
      out.hkdf = hex(await crypto.subtle.deriveBits({{ name: 'HKDF', hash: 'SHA-256', salt: unhex('000102030405060708090a0b0c'), info: unhex('f0f1f2f3f4f5f6f7f8f9') }}, ikm, 336));
      const aesFromPw = await crypto.subtle.deriveKey({{ name: 'PBKDF2', hash: 'SHA-256', salt: enc('salt'), iterations: 1 }}, pw, {{ name: 'AES-GCM', length: 256 }}, true, ['encrypt']);
      out.derivedAes = hex(await crypto.subtle.exportKey('raw', aesFromPw));
      try {{ await crypto.subtle.exportKey('raw', pw); }} catch (e) {{ out.notExtractable = e.name; }}
      try {{ await crypto.subtle.digest('MD5', enc('x')); }} catch (e) {{ out.md5 = e.name; }}
    "#
    ));
    assert_eq!(
        get(&mut page, "sha256"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(
        get(&mut page, "hmac"),
        "f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8"
    );
    assert_eq!(get(&mut page, "hmacOk"), "true");
    assert_eq!(get(&mut page, "hmacBad"), "false");
    assert_eq!(get(&mut page, "jwk"), "oct HS256 a2V5 true sign,verify");
    assert_eq!(
        get(&mut page, "alg"),
        "{\"name\":\"HMAC\",\"hash\":{\"name\":\"SHA-256\"},\"length\":24} secret sign,verify"
    );
    assert_eq!(
        get(&mut page, "pbkdf2"),
        "120fb6cffcf8b32c43e7225256c4f837a86548c92ccc35480805987cb70be17b"
    );
    assert_eq!(
        get(&mut page, "hkdf"),
        "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865"
    );
    assert_eq!(
        get(&mut page, "derivedAes"),
        "120fb6cffcf8b32c43e7225256c4f837a86548c92ccc35480805987cb70be17b"
    );
    assert_eq!(get(&mut page, "notExtractable"), "InvalidAccessError");
    assert_eq!(get(&mut page, "md5"), "NotSupportedError");
}

#[test]
fn aes_round_trips_in_every_mode() {
    let mut page = run(&format!(
        r#"{HELPERS}
      const key = await crypto.subtle.generateKey({{ name: 'AES-GCM', length: 128 }}, true, ['encrypt', 'decrypt']);
      out.len = (await crypto.subtle.exportKey('raw', key)).byteLength + ' ' + key.algorithm.length + ' ' + key.extractable;
      const iv = crypto.getRandomValues(new Uint8Array(12));
      const ct = await crypto.subtle.encrypt({{ name: 'AES-GCM', iv, additionalData: enc('aad') }}, key, enc('secret message'));
      out.gcmLen = ct.byteLength;
      out.gcm = new TextDecoder().decode(await crypto.subtle.decrypt({{ name: 'AES-GCM', iv, additionalData: enc('aad') }}, key, ct));
      try {{ await crypto.subtle.decrypt({{ name: 'AES-GCM', iv }}, key, ct); }} catch (e) {{ out.gcmBadAad = e.name; }}
      const cbcKey = await crypto.subtle.importKey('raw', unhex('2b7e151628aed2a6abf7158809cf4f3c'), 'AES-CBC', false, ['encrypt', 'decrypt']);
      const cbcIv = unhex('000102030405060708090a0b0c0d0e0f');
      const cbc = await crypto.subtle.encrypt({{ name: 'AES-CBC', iv: cbcIv }}, cbcKey, unhex('6bc1bee22e409f96e93d7e117393172a'));
      out.cbc = hex(cbc);
      out.cbcBack = hex(await crypto.subtle.decrypt({{ name: 'AES-CBC', iv: cbcIv }}, cbcKey, cbc));
      const ctrKey = await crypto.subtle.importKey('raw', unhex('2b7e151628aed2a6abf7158809cf4f3c'), 'AES-CTR', false, ['encrypt', 'decrypt']);
      const ctr = await crypto.subtle.encrypt({{ name: 'AES-CTR', counter: unhex('f0f1f2f3f4f5f6f7f8f9fafbfcfdfeff'), length: 64 }}, ctrKey, unhex('6bc1bee22e409f96e93d7e117393172a'));
      out.ctr = hex(ctr);
      try {{ await crypto.subtle.encrypt({{ name: 'AES-GCM', iv }}, cbcKey, enc('x')); }} catch (e) {{ out.wrongAlg = e.name; }}
    "#
    ));
    assert_eq!(get(&mut page, "len"), "16 128 true");
    assert_eq!(get(&mut page, "gcmLen"), "30");
    assert_eq!(get(&mut page, "gcm"), "secret message");
    assert_eq!(get(&mut page, "gcmBadAad"), "OperationError");
    assert_eq!(
        get(&mut page, "cbc"),
        "7649abac8119b246cee98e9b12e9197d8964e0b149c10b7b682e6e39aaeb731c"
    );
    assert_eq!(
        get(&mut page, "cbcBack"),
        "6bc1bee22e409f96e93d7e117393172a"
    );
    assert_eq!(get(&mut page, "ctr"), "874d6191b620e3261bef6864990db6ce");
    assert_eq!(get(&mut page, "wrongAlg"), "InvalidAccessError");
}

#[test]
fn asymmetric_keys_sign_agree_and_round_trip_formats() {
    let mut page = run(&format!(
        r#"{HELPERS}
      const ec = await crypto.subtle.generateKey({{ name: 'ECDSA', namedCurve: 'P-256' }}, true, ['sign', 'verify']);
      out.ecTypes = ec.publicKey.type + ' ' + ec.privateKey.type + ' ' + ec.publicKey.usages + ' ' + ec.privateKey.usages + ' ' + ec.privateKey.algorithm.namedCurve;
      const sig = await crypto.subtle.sign({{ name: 'ECDSA', hash: 'SHA-256' }}, ec.privateKey, enc('msg'));
      out.sigLen = sig.byteLength;
      const spki = await crypto.subtle.exportKey('spki', ec.publicKey);
      const pub2 = await crypto.subtle.importKey('spki', spki, {{ name: 'ECDSA', namedCurve: 'P-256' }}, true, ['verify']);
      out.ecOk = await crypto.subtle.verify({{ name: 'ECDSA', hash: 'SHA-256' }}, pub2, sig, enc('msg'));
      out.ecBad = await crypto.subtle.verify({{ name: 'ECDSA', hash: 'SHA-256' }}, pub2, sig, enc('other'));
      const pkcs8 = await crypto.subtle.exportKey('pkcs8', ec.privateKey);
      const priv2 = await crypto.subtle.importKey('pkcs8', pkcs8, {{ name: 'ECDSA', namedCurve: 'P-256' }}, true, ['sign']);
      const jwk = await crypto.subtle.exportKey('jwk', priv2);
      out.ecJwk = jwk.kty + ' ' + jwk.crv + ' ' + (jwk.d.length > 40) + ' ' + jwk.x.length;
      const raw = await crypto.subtle.exportKey('raw', ec.publicKey);
      out.rawLen = raw.byteLength;

      const a = await crypto.subtle.generateKey({{ name: 'ECDH', namedCurve: 'P-256' }}, false, ['deriveBits']);
      const b = await crypto.subtle.generateKey({{ name: 'ECDH', namedCurve: 'P-256' }}, false, ['deriveBits']);
      const ab = hex(await crypto.subtle.deriveBits({{ name: 'ECDH', public: b.publicKey }}, a.privateKey, 256));
      const ba = hex(await crypto.subtle.deriveBits({{ name: 'ECDH', public: a.publicKey }}, b.privateKey, 256));
      out.ecdh = (ab === ba) + ' ' + ab.length;

      const edPriv = await crypto.subtle.importKey('jwk', {{ kty: 'OKP', crv: 'Ed25519', d: 'nWGxne_9WmC6hEr0kuwsxERJxWl7MmkZcDusAxyuf2A', x: '11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo' }}, 'Ed25519', true, ['sign']);
      out.ed = hex(await crypto.subtle.sign('Ed25519', edPriv, new Uint8Array(0)));
      const edPub = await crypto.subtle.importKey('raw', unhex('d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a'), 'Ed25519', true, ['verify']);
      out.edOk = await crypto.subtle.verify('Ed25519', edPub, unhex(out.ed), new Uint8Array(0));
      const edSpki = await crypto.subtle.exportKey('spki', edPub);
      out.edSpki = hex(edSpki);
      const edPkcs8 = await crypto.subtle.exportKey('pkcs8', edPriv);
      const edBack = await crypto.subtle.importKey('pkcs8', edPkcs8, 'Ed25519', true, ['sign']);
      out.edBack = (await crypto.subtle.exportKey('jwk', edBack)).x;

      const xa = await crypto.subtle.importKey('raw', unhex('de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f'), 'X25519', false, []);
      const xb = await crypto.subtle.importKey('jwk', {{ kty: 'OKP', crv: 'X25519', d: 'dwdtCnMYpX08FsFyUbJmRd9ML4frwJkqsXf7pR25LCo', x: 'hSDwCYkwp1R0i33ctD73Wg2_Og0mOBr066SpjqqbTmo' }}, 'X25519', false, ['deriveBits']);
      out.x25519 = hex(await crypto.subtle.deriveBits({{ name: 'X25519', public: xa }}, xb, 256));

      const rsa = await crypto.subtle.generateKey({{ name: 'RSA-OAEP', modulusLength: 1024, publicExponent: new Uint8Array([1, 0, 1]), hash: 'SHA-256' }}, true, ['encrypt', 'decrypt']);
      const rct = await crypto.subtle.encrypt({{ name: 'RSA-OAEP' }}, rsa.publicKey, enc('hello rsa'));
      out.rsaLen = rct.byteLength + ' ' + rsa.publicKey.algorithm.modulusLength + ' ' + rsa.privateKey.usages;
      out.rsa = new TextDecoder().decode(await crypto.subtle.decrypt({{ name: 'RSA-OAEP' }}, rsa.privateKey, rct));
      const rjwk = await crypto.subtle.exportKey('jwk', rsa.privateKey);
      out.rsaJwk = rjwk.kty + ' ' + rjwk.alg + ' ' + ['n','e','d','p','q','dp','dq','qi'].every(k => typeof rjwk[k] === 'string');
      const rsaBack = await crypto.subtle.importKey('jwk', rjwk, {{ name: 'RSA-OAEP', hash: 'SHA-256' }}, false, ['decrypt']);
      out.rsaBack = new TextDecoder().decode(await crypto.subtle.decrypt({{ name: 'RSA-OAEP' }}, rsaBack, rct));
      const rs = await crypto.subtle.generateKey({{ name: 'RSASSA-PKCS1-v1_5', modulusLength: 1024, publicExponent: new Uint8Array([1, 0, 1]), hash: 'SHA-256' }}, false, ['sign', 'verify']);
      const rsig = await crypto.subtle.sign('RSASSA-PKCS1-v1_5', rs.privateKey, enc('doc'));
      out.rsaSig = (await crypto.subtle.verify('RSASSA-PKCS1-v1_5', rs.publicKey, rsig, enc('doc'))) + ' ' + rsig.byteLength;
      const ps = await crypto.subtle.generateKey({{ name: 'RSA-PSS', modulusLength: 1024, publicExponent: new Uint8Array([1, 0, 1]), hash: 'SHA-256' }}, false, ['sign', 'verify']);
      const psig = await crypto.subtle.sign({{ name: 'RSA-PSS', saltLength: 32 }}, ps.privateKey, enc('doc'));
      out.pss = (await crypto.subtle.verify({{ name: 'RSA-PSS', saltLength: 32 }}, ps.publicKey, psig, enc('doc'))) + ' ' + (await crypto.subtle.verify({{ name: 'RSA-PSS', saltLength: 32 }}, ps.publicKey, psig, enc('other')));
    "#
    ));
    assert_eq!(
        get(&mut page, "ecTypes"),
        "public private verify sign P-256"
    );
    assert_eq!(get(&mut page, "sigLen"), "64");
    assert_eq!(get(&mut page, "ecOk"), "true");
    assert_eq!(get(&mut page, "ecBad"), "false");
    assert_eq!(get(&mut page, "ecJwk"), "EC P-256 true 43");
    assert_eq!(get(&mut page, "rawLen"), "65");
    assert_eq!(get(&mut page, "ecdh"), "true 64");
    assert_eq!(
        get(&mut page, "ed"),
        "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b"
    );
    assert_eq!(get(&mut page, "edOk"), "true");
    assert_eq!(
        get(&mut page, "edSpki"),
        "302a300506032b6570032100d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a"
    );
    assert_eq!(
        get(&mut page, "edBack"),
        "11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo"
    );
    assert_eq!(
        get(&mut page, "x25519"),
        "4a5d9d5ba4ce2de1728e3bf480350f25e07e21c947d19e3376f09b3c1e161742"
    );
    assert_eq!(get(&mut page, "rsaLen"), "128 1024 decrypt");
    assert_eq!(get(&mut page, "rsa"), "hello rsa");
    assert_eq!(get(&mut page, "rsaJwk"), "RSA RSA-OAEP-256 true");
    assert_eq!(get(&mut page, "rsaBack"), "hello rsa");
    assert_eq!(get(&mut page, "rsaSig"), "true 128");
    assert_eq!(get(&mut page, "pss"), "true false");
}

// Same challenge-response login as the main Ground Station UI. Tokens stay in memory.
export async function signIn(username, password) {
  if (!crypto.subtle) throw new Error('Sign-in and microphone access require HTTPS (or localhost).');
  username = username.trim().toLowerCase();
  const encode = value => new TextEncoder().encode(value);
  const b64 = value => Uint8Array.from(atob(value), c => c.charCodeAt(0));
  const urlB64 = value => btoa(String.fromCharCode(...new Uint8Array(value))).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
  const post = async (path, body) => {
    const response = await fetch(path, {method:'POST', headers:{'Content-Type':'application/json'}, body:JSON.stringify(body)});
    if (!response.ok) throw new Error(await response.text() || `HTTP ${response.status}`);
    return response.json();
  };
  const challenge = await post('/api/auth/challenge', {username});
  if (challenge.algorithm !== 'pbkdf2_sha256') throw new Error('Unsupported password algorithm');
  const key = await crypto.subtle.importKey('raw', encode(password), 'PBKDF2', false, ['deriveBits']);
  const verifier = await crypto.subtle.deriveBits({name:'PBKDF2', hash:'SHA-256', salt:b64(challenge.salt_b64), iterations:challenge.iterations}, key, 256);
  const client_nonce_b64 = urlB64(crypto.getRandomValues(new Uint8Array(32)));
  const payload = {username, challenge_id:challenge.challenge_id, server_nonce_b64:challenge.server_nonce_b64, client_nonce_b64, remember_me:false};
  const signingKey = await crypto.subtle.importKey('raw', verifier, {name:'HMAC', hash:'SHA-256'}, false, ['sign']);
  const proof_b64 = urlB64(await crypto.subtle.sign('HMAC', signingKey, encode(JSON.stringify(payload))));
  return post('/api/auth/login', {username, challenge_id:challenge.challenge_id, client_nonce_b64, remember_me:false, proof_b64});
}

const TOTAL = document.querySelectorAll('.slide').length;
let current = 0;

const slides    = () => document.querySelectorAll('.slide');
const dotsEl    = () => document.querySelectorAll('.dot');
const prevBtn   = document.getElementById('prevBtn');
const nextBtn   = document.getElementById('nextBtn');
const counter   = document.getElementById('slide-counter');
const badge     = document.getElementById('slide-badge');
const progress  = document.getElementById('progress-bar');

function go(n) {
  const all = slides();
  const old = current;
  all[old].classList.remove('active');
  all[old].classList.add('exit');
  setTimeout(() => all[old].classList.remove('exit'), 600);

  current = Math.max(0, Math.min(n, TOTAL - 1));

  all[current].classList.add('active');
  sync();
}

function sync() {
  counter.textContent = `${current + 1} / ${TOTAL}`;
  badge.textContent   = `${current + 1} / ${TOTAL}`;
  prevBtn.disabled    = current === 0;
  nextBtn.disabled    = current === TOTAL - 1;
  progress.style.width = `${((current + 1) / TOTAL) * 100}%`;
  dotsEl().forEach((d, i) => d.classList.toggle('active', i === current));
}

document.getElementById('prevBtn').onclick = () => go(current - 1);
document.getElementById('nextBtn').onclick = () => go(current + 1);

document.addEventListener('keydown', e => {
  if (e.key === 'ArrowRight' || e.key === 'ArrowDown' || e.key === ' ') { e.preventDefault(); go(current + 1); }
  if (e.key === 'ArrowLeft'  || e.key === 'ArrowUp')                    { e.preventDefault(); go(current - 1); }
});

// Build dots
const dotsContainer = document.querySelector('.dots');
for (let i = 0; i < TOTAL; i++) {
  const d = document.createElement('div');
  d.className = 'dot' + (i === 0 ? ' active' : '');
  d.onclick = () => go(i);
  dotsContainer.appendChild(d);
}

// Init first slide
slides()[0].classList.add('active');
sync();

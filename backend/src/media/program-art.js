// Reuse the same 2D artwork: large without video, compact beside a live camera.
window.gsProgramArt = {
  layout(holder, inset, footer, flat) {
    const destination = flat ? holder : footer;
    if (inset.parentElement !== destination) {
      if (flat) destination.appendChild(inset);
      else destination.insertBefore(inset, destination.firstChild);
    }
    holder.dataset.flat = String(flat);
    footer.dataset.rocketMain = String(flat);
  }
};
